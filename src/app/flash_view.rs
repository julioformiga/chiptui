//! `esptool` flash-view dispatch: the menu, options screen, online
//! board/firmware search, and everything that decides when a destructive
//! action needs confirmation. Split out of `app.rs` since it is the one
//! subsystem `App` drives almost entirely through [`crate::flash::FlashPanel`]
//! and never touches [`crate::browser`] directly (beyond checking whether it
//! still holds the serial port).

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::Capability;
use crate::browser::{Browser, PaneState};
use crate::device::ScriptState;
use crate::firmware_id::{FirmwareVerdict, FlashFirmware};
use crate::flash::{FlashAction, FlashPaneAction, FlashPanel, FlashScreen, OptionsField, RunState};

use super::{App, DevicePaneTab, Focus, LogTab, MonitorSource, Overlay, View};

/// What trying to start the deferred device query concluded, so callers
/// that chained something behind it (the first device listing,
/// [`App::hold_root_listing_for_chip_identity`]) know whether to keep
/// waiting or move on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeferredQuery {
    /// A guard still holds it (an open overlay, a busy port holder, a
    /// script believed running); it stays pending and will be retried.
    Waiting,
    /// The query is running; its `FlashUpdate` finish flag will follow.
    Started,
    /// The query was consumed but could not start (a manual esptool
    /// command owns the panel, or the port vanished) --- nothing will
    /// follow it.
    Dropped,
}

/// Where the firmware-identification read stands for the port it was
/// armed for (`App::firmware_check_port`). The read runs as part of the
/// probe → chip identity → firmware → listing chain a device selection
/// starts, so the first listing waits on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum FirmwareCheck {
    #[default]
    Idle,
    /// Armed, waiting for every port holder to be gone; the tick (and
    /// every process event) polls it onto the port.
    Pending,
    /// The `esptool read-flash` is running; its finish event moves the
    /// check back to `Idle` with the verdict in the flash panel.
    Running,
}

/// Why the first device listing may or may not proceed past the firmware
/// identification ([`App::hold_root_listing_for_firmware`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FirmwareHold {
    /// MicroPython is confirmed, identification cannot run for this board
    /// (the chip query never succeeded), or the read concluded without a
    /// recognizable verdict: list, and let mpremote speak for itself.
    Release,
    /// The identification read owns the ordering: the listing waits for
    /// its verdict.
    Held,
    /// The flash says the board runs something other than MicroPython:
    /// there is no filesystem to list, and the device pane already
    /// carries the reason.
    Blocked,
}

impl App {
    /// `q`/esc step back one screen (Options/Output to Menu) rather than
    /// leaving straight to the dashboard, mirroring the file browser's "do
    /// not throw work away by reflex" rule --- except from the top-level
    /// menu, where there is nowhere closer to go.
    pub(super) fn on_flash_key(&mut self, key: KeyEvent) {
        let Some(screen) = self.flash.as_ref().map(|flash| flash.screen) else {
            return;
        };

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                match screen {
                    FlashScreen::Menu => self.view = View::Dashboard,
                    FlashScreen::Options
                    | FlashScreen::OnlineBoards
                    | FlashScreen::OnlineFirmware
                    | FlashScreen::CustomUrl => self.leave_flash_screen(),
                }
                return;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.overlay = Some(crate::app::OVERLAY_HELP);
                return;
            }
            _ => {}
        }

        match screen {
            FlashScreen::Menu => self.on_flash_menu_key(key),
            FlashScreen::Options => self.on_flash_options_key(key),
            FlashScreen::OnlineBoards => self.on_flash_online_boards_key(key),
            FlashScreen::OnlineFirmware => self.on_flash_online_firmware_key(key),
            FlashScreen::CustomUrl => self.on_flash_custom_url_key(key),
        }
    }

    /// One screen back from a dialog screen (options, the online windows,
    /// the URL entry). The menu those screens sit over is the device
    /// pane's **Project actions** tab now, so wherever the pane hosts it
    /// "back" is the dashboard itself: stepping to [`FlashScreen::Menu`]
    /// would layer a second copy of the very actions the pane behind the
    /// dialog is already showing. Only a backend with no pane to host
    /// them still has the dialog menu to step back to.
    fn leave_flash_screen(&mut self) {
        if self.device_actions_tab_available() {
            self.view = View::Dashboard;
        }
        if let Some(flash) = &mut self.flash {
            flash.screen = FlashScreen::Menu;
        }
    }

    /// Layers the flash dialog over the dashboard for the screen the panel
    /// just moved to. A refused step --- a search with no chip known, no
    /// curl, or a command already running --- leaves the panel on
    /// [`FlashScreen::Menu`], which is the screen the device pane's own tab
    /// already *is*: there is nothing to open, and the warning in the log is
    /// the whole answer.
    fn show_flash_dialog(&mut self) {
        let screen = self.flash.as_ref().map(|flash| flash.screen);
        if screen == Some(FlashScreen::Menu) && self.device_actions_tab_available() {
            return;
        }
        self.view = View::Flash;
    }

    /// Handles a key while the device pane's **Project actions** tab holds
    /// focus: navigated like the build panel's list (`j`/`k`, arrows, page,
    /// home/end) and `Enter` runs the row under the cursor --- `Stop` while
    /// a command is running. The tab strip's own arrows (switching the
    /// pane's tabs) are handled one level up, with the dashboard dispatch:
    /// they switch from either side, row 3's rule.
    pub(super) fn on_flash_pane_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let len = flash.pane_actions().len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                flash.pane_cursor = flash.pane_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                flash.pane_cursor = (flash.pane_cursor + 1).min(len - 1);
            }
            KeyCode::PageUp => flash.pane_cursor = flash.pane_cursor.saturating_sub(5),
            KeyCode::PageDown => flash.pane_cursor = (flash.pane_cursor + 5).min(len - 1),
            KeyCode::Home => flash.pane_cursor = 0,
            KeyCode::End => flash.pane_cursor = len - 1,
            KeyCode::Enter => {
                let action = flash.pane_action_at(flash.pane_cursor);
                self.flash = Some(flash);
                if let Some(action) = action {
                    self.run_flash_pane_action(action);
                }
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    /// Runs the row the actions tab's cursor sits on: an esptool action
    /// through the same gate the menu always used (firmware pick or
    /// confirmation first, `SPEC.md` §15), the online-firmware search as
    /// its dialog, `Stop` as a cancel. Also the click path
    /// (`app::mouse`): a click on the tab's button row lands here, the
    /// same way `Enter` does.
    pub(super) fn run_flash_pane_action(&mut self, action: FlashPaneAction) {
        match action {
            FlashPaneAction::Stop => self.stop_flash(),
            FlashPaneAction::Run(action) => self.trigger_flash_action(action),
            FlashPaneAction::SearchOnline => {
                self.search_online();
                self.show_flash_dialog();
            }
        }
    }

    /// Cancels whatever the panel is running at the user's request (the
    /// tab's `Stop` button) --- the row is offered for every busy state
    /// ([`FlashPanel::is_busy`]), so it reaches every one of them.
    fn stop_flash(&mut self) {
        let Some(flash) = &mut self.flash else {
            return;
        };
        if flash.stop(&mut self.processes) {
            // Whatever the row was offered for: the esptool command, one of
            // the background queries, or a curl fetch.
            self.logs.warn("stopping the running command");
        }
    }

    fn on_flash_menu_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => flash.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => flash.move_cursor(1),
            KeyCode::Char('s') => {
                self.flash = Some(flash);
                self.search_online();
                return;
            }
            KeyCode::Char('u') => {
                flash.custom_url.clear();
                flash.screen = FlashScreen::CustomUrl;
            }
            KeyCode::Enter => {
                let action = flash.selected_action();
                self.flash = Some(flash);
                self.trigger_flash_action(action);
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    /// Searches micropython.org/download/ for the currently known chip
    /// (`SPEC.md` §9) --- by MCU alone, so every vendor's boards arrive and
    /// the selection table's `Vendor` column tells them apart. Local
    /// candidates are re-discovered first so the search window's
    /// local-folder note starts truthful, and the source URL is logged so
    /// the feed's origin is on record outside the window too.
    pub(super) fn search_online(&mut self) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let Some(mcu) = flash
            .chip
            .family()
            .map(|family| family.micropython_mcu_filter())
        else {
            self.flash = Some(flash);
            self.logs
                .warn("no chip known yet --- connect the board or pick one in Options first");
            return;
        };
        // Refresh the local candidates so the search window's local-folder
        // note starts truthful (the `s` key can arrive before any discovery
        // ever ran). Silent by design: every other path here (write/flash,
        // flash-info) has just logged its own discovery notice, and the
        // window itself states the folder's state --- only a genuinely
        // unreadable directory is worth a second log line.
        let mut notices = flash
            .discover_firmware()
            .into_iter()
            .filter(|(level, _)| *level == crate::logs::Level::Error)
            .collect::<Vec<_>>();
        notices.extend(flash.search_online(mcu, &mut self.processes));
        let source = flash.online_source.clone();
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
        if let Some(url) = source {
            self.logs
                .info(format!("searching {url} for {mcu} firmware"));
        }
    }

    fn on_flash_online_boards_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let count = flash.online_boards.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => flash.move_online_cursor(-1, count),
            KeyCode::Down | KeyCode::Char('j') => flash.move_online_cursor(1, count),
            KeyCode::Char('u') => {
                flash.custom_url.clear();
                flash.screen = FlashScreen::CustomUrl;
            }
            KeyCode::Enter => {
                let Some(board_id) = flash
                    .online_boards
                    .get(flash.online_cursor)
                    .map(|board| board.id.clone())
                else {
                    self.flash = Some(flash);
                    return;
                };
                self.flash = Some(flash);
                self.fetch_selected_board(&board_id);
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    fn on_flash_online_firmware_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let count = flash.online_firmware.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => flash.move_online_cursor(-1, count),
            KeyCode::Down | KeyCode::Char('j') => flash.move_online_cursor(1, count),
            KeyCode::Char('u') => {
                flash.custom_url.clear();
                flash.screen = FlashScreen::CustomUrl;
            }
            KeyCode::Enter => {
                let url = flash
                    .online_firmware
                    .get(flash.online_cursor)
                    .map(|file| file.url.clone());
                self.flash = Some(flash);
                if let Some(url) = url {
                    self.request_download(url);
                }
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    fn on_flash_custom_url_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        match key.code {
            KeyCode::Backspace => flash.backspace_custom_url(),
            KeyCode::Char(c) => flash.push_custom_url_char(c),
            KeyCode::Enter => {
                let url = flash.custom_url.trim().to_string();
                self.flash = Some(flash);
                if url.is_empty() {
                    self.logs.warn("type or paste a URL first");
                } else {
                    self.request_download(url);
                }
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    fn fetch_selected_board(&mut self, board_id: &str) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let notices = flash.fetch_board_page(board_id, &mut self.processes);
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    /// Starts a download, first asking for confirmation if it would
    /// overwrite a file already in `firmware/` (`SPEC.md` §15).
    fn request_download(&mut self, url: String) {
        let Some(flash) = &self.flash else {
            return;
        };
        let Some(dest) = flash.download_destination(&url) else {
            self.logs.warn("cannot determine a filename from that URL");
            return;
        };
        if dest.exists() {
            self.overlay = Some(Overlay::ConfirmDownloadOverwrite {
                url,
                dest,
                confirm: false,
            });
        } else {
            self.start_download(url, dest);
        }
    }

    pub(super) fn start_download(&mut self, url: String, dest: PathBuf) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let notices = flash.download(&url, dest, &mut self.processes);
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    fn on_flash_options_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let action = flash.selected_action();

        match key.code {
            KeyCode::Tab => flash.step_options_focus(action, true),
            KeyCode::BackTab => flash.step_options_focus(action, false),
            KeyCode::Left => self.step_option_value(&mut flash, false),
            KeyCode::Right => self.step_option_value(&mut flash, true),
            KeyCode::Backspace => match flash.options_focus {
                OptionsField::Offset => flash.backspace_offset(),
                OptionsField::ExtraArgs => flash.backspace_extra_args(),
                _ => {}
            },
            KeyCode::Char(c) => match flash.options_focus {
                OptionsField::Offset => flash.push_offset_char(c),
                OptionsField::ExtraArgs => flash.push_extra_arg_char(c),
                _ => {}
            },
            KeyCode::Enter => {
                self.flash = Some(flash);
                self.trigger_flash_action(action);
                return;
            }
            _ => {}
        }

        self.flash = Some(flash);
    }

    /// `Left`/`Right` only do something on a cyclable field --- text fields
    /// (offset, custom flags) are edited by typing, not arrowing through.
    fn step_option_value(&self, flash: &mut FlashPanel, forward: bool) {
        match flash.options_focus {
            OptionsField::Chip => flash.cycle_chip(forward),
            OptionsField::FlashMode => flash.cycle_flash_mode(forward),
            OptionsField::FlashFreq => flash.cycle_flash_freq(forward),
            OptionsField::FlashSize => flash.cycle_flash_size(forward),
            OptionsField::Offset | OptionsField::ExtraArgs => {}
        }
    }

    /// Starts `action`, or gathers what it needs first: a firmware file
    /// (`FlashAction::needs_firmware`) or explicit confirmation
    /// (`FlashAction::is_destructive`, `SPEC.md` §15). Shared by the menu and
    /// the options screen, and by the automatic post-erase offer.
    fn trigger_flash_action(&mut self, action: FlashAction) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };

        if action.needs_firmware() && flash.selected_firmware.is_none() {
            let notices = flash.discover_firmware();
            for (level, message) in notices {
                self.logs.push(level, message);
            }
            match flash.firmware.len() {
                // Nothing local to flash: rather than dead-ending with a
                // warning, write/flash opens the online search window for
                // the detected chip --- the source it is querying and the
                // fact that a file dropped into firmware/ outranks it are
                // both on that window (`SPEC.md` §9).
                0 if action == FlashAction::WriteFlash => {
                    self.flash = Some(flash);
                    self.search_online();
                    self.show_flash_dialog();
                }
                0 => {
                    self.flash = Some(flash);
                }
                1 => {
                    flash.screen = FlashScreen::Options;
                    flash.options_focus = OptionsField::Chip;
                    self.flash = Some(flash);
                }
                _ => {
                    self.overlay = Some(Overlay::FirmwarePicker { selected: 0 });
                    self.flash = Some(flash);
                }
            }
            return;
        }

        if let Some(reason) = flash.blocked_reason(action) {
            self.logs.warn(reason);
            self.flash = Some(flash);
            return;
        }

        let port = self.devices.selected_port().map(str::to_string);
        if action.is_destructive() {
            flash.request_confirmation(action);
            let message = flash
                .command_preview(action, port.as_deref())
                .unwrap_or_else(|| "(command unavailable)".to_string());
            self.overlay = Some(Overlay::Confirm {
                message,
                confirm: false,
            });
            self.flash = Some(flash);
        } else {
            let notices = flash.run(action, &mut self.processes, port.as_deref());
            self.flash = Some(flash);
            if notices.is_empty() {
                self.show_flash_in_monitor();
            }
            for (level, message) in notices {
                self.logs.push(level, message);
            }
        }
    }

    /// Closes any open flash dialog and points the Monitor tab at the
    /// streamed output ([`FlashPanel::output`]) --- called after a flash
    /// action successfully spawns. Where the user *sits* while it runs
    /// follows the pane: the actions tab keeps them (the Zephyr build
    /// pane's rule --- the Monitor tab is shown, never focused, and the
    /// cursor is already parked on `Stop` by the spawn); anything else
    /// (a dialog-started run over the files tab) falls back to focusing
    /// the Monitor tab, the behavior the dialog always had.
    pub(super) fn show_flash_in_monitor(&mut self) {
        self.view = View::Dashboard;
        self.log_tab = LogTab::Monitor;
        self.set_monitor_source(MonitorSource::Flash);
        if self.device_actions_tab_active() {
            self.focus = Focus::FilesDevice;
        } else {
            self.focus = Focus::Logs;
        }
    }

    /// Opens the flash actions, if the selected backend can flash or erase.
    ///
    /// The gate is the capability, never the backend kind (`AGENTS.md` §3).
    /// A backend whose flashing lives in the build panel (`west flash`
    /// behind a confirm, e.g. Zephyr) is routed there instead of esptool's
    /// actions. A backend with a device pane (filesystem + esptool) gets
    /// the **Project actions** tab: the pane *is* the menu now, so `x`
    /// switches the tab and focuses it, and only the options/online screens
    /// remain dialogs. Anything else (no browser to host the tab) keeps the
    /// dialog form.
    pub fn open_flash(&mut self) {
        if self.build_pane_visible() {
            self.run_build_action(crate::build::BuildAction::Flash);
            return;
        }
        if !self.ensure_flash_panel() {
            let backend = self
                .manager
                .selected_kind()
                .map_or("no backend".to_string(), |kind| kind.to_string());
            self.logs
                .warn(format!("{backend} does not expose flash operations"));
            return;
        }
        if self.device_actions_tab_available() {
            self.show_device_actions_tab();
            self.focus = Focus::FilesDevice;
            return;
        }
        self.view = View::Flash;
    }

    /// Switches the device pane to its **Project actions** tab, creating
    /// the flash panel the tab draws if nothing has yet ([`x`
    /// →`Self::open_flash`] is not the only way in: the strip's own arrows
    /// reach the tab too, and a background query may never have run ---
    /// with no board plugged in, nothing else creates the panel). An
    /// actions tab without one would draw an empty pane over a row sized
    /// for no content at all. The capability gate is the one
    /// [`App::device_actions_tab_available`] passes before every call, so
    /// it holds here.
    pub(super) fn show_device_actions_tab(&mut self) {
        if self.ensure_flash_panel() {
            self.device_pane_tab = DevicePaneTab::Actions;
        }
    }

    /// Capability-gates and lazily creates [`Self::flash`] --- the shared
    /// prerequisite between opening the Flash view by hand and
    /// [`App::maybe_query_device_info`]'s background refresh. `false` means
    /// the backend has neither `Flash` nor `EraseFlash`.
    fn ensure_flash_panel(&mut self) -> bool {
        let caps = self.manager.capabilities();
        if !caps.contains(Capability::Flash) && !caps.contains(Capability::EraseFlash) {
            return false;
        }
        if self.flash.is_none() {
            let root = self
                .manager
                .root()
                .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
            // The `firmware/` directory belongs to the esptool flow
            // (MicroPython). A backend whose flash capability is another
            // tool's (`west flash`, e.g. Zephyr) gets this panel purely as
            // the identity-query engine --- its project tree must not grow
            // an unused `firmware/` directory for it.
            if caps.contains(Capability::DeviceInfo) || caps.contains(Capability::EraseFlash) {
                let firmware_dir = root.join("firmware");
                // Lazily created here rather than only via `ensure_micropython_layout`
                // so opening Flash still works for a project that never went
                // through the empty-project prompt (e.g. an existing MicroPython
                // project detected automatically, `SPEC.md` §7).
                if let Err(err) = std::fs::create_dir_all(&firmware_dir) {
                    self.logs.warn(format!(
                        "could not create {}: {err}",
                        firmware_dir.display()
                    ));
                }
                self.flash = Some(FlashPanel::new(firmware_dir));
            } else {
                self.flash = Some(FlashPanel::new(root));
            }
        }
        true
    }

    /// Marks a background chip query as due, without starting it yet ---
    /// `esptool` cannot open the serial port while `mpremote` (just kicked
    /// off by `load_device_root`, right before every call site of this
    /// method) still holds it exclusively. The query is the board's
    /// identity read (`esptool chip-id`), and the first listing of a newly
    /// selected device is held behind it ([`Self::hold_root_listing_for_chip_identity`]),
    /// so `mpremote devs` → probe → chip-id → firmware read → listing is
    /// the one order in which the port changes hands cleanly.
    /// `maybe_run_deferred_flash_query` starts the query for real once
    /// every port holder is gone.
    pub(super) fn defer_device_info_query(&mut self) {
        self.flash_query_pending = true;
    }

    /// Runs the deferred chip query once it is actually safe to, polled on
    /// every tick and after each process event (a held request queue
    /// produces no events, so the tick is what keeps a declined
    /// interruption from stranding the query forever).
    ///
    /// "Safe" means more than a free port: `esptool` resets the board into
    /// its bootloader to read the chip, which stops a running script just
    /// like an `mpremote` interrupt does --- so a script believed running
    /// postpones the query too, as does any open overlay (the user may be a
    /// keypress away from confirming something that also wants the port).
    /// An accepted interruption no longer does: the restore question it
    /// arms deliberately waits for this query (and the identification read
    /// behind it) to finish before opening, so blocking on it would
    /// deadlock the chain --- and the reset the query performs is exactly
    /// the interruption the user just accepted.
    pub(super) fn maybe_run_deferred_flash_query(&mut self) -> DeferredQuery {
        if !self.flash_query_pending
            || self.overlay.is_some()
            || self.probe.is_some()
            || self.device_monitor_process.is_some()
            || self.run_process.is_some()
            || self.browser.as_ref().is_some_and(Browser::is_busy)
            || self.devices.script_state() == ScriptState::Running
        {
            return DeferredQuery::Waiting;
        }
        self.flash_query_pending = false;
        if self.maybe_query_device_info() {
            DeferredQuery::Started
        } else {
            DeferredQuery::Dropped
        }
    }

    /// Kicks off a background `esptool chip-id` so the Dashboard's device
    /// panel has something to show without the user ever opening the Flash
    /// view --- same shape as [`App::maybe_scan_devices`]'s "load this
    /// eagerly, once the prerequisite is known", just for esptool instead
    /// of mpremote. Only ever called once [`Self::defer_device_info_query`]'s
    /// wait for `mpremote` to release the port is satisfied. Returns
    /// whether the query started; a no-op `false` when the backend has no
    /// flash capability or a flash command is already running (the caller
    /// decides whether anyone was waiting on it).
    /// [`FlashPanel::query_device_info`]'s synchronous rejection notice is
    /// swallowed on purpose --- a courtesy refresh finding the panel busy is
    /// not something the user needs to hear about, unlike its eventual
    /// success or failure, which still reaches the log the normal way once
    /// the process finishes.
    pub(super) fn maybe_query_device_info(&mut self) -> bool {
        if !self.ensure_flash_panel() {
            return false;
        }
        let Some(port) = self.devices.selected_port().map(str::to_string) else {
            return false;
        };
        let Some(mut flash) = self.flash.take() else {
            return false;
        };
        let started = flash.query_device_info(&mut self.processes, Some(&port));
        self.flash = Some(flash);
        started
    }

    /// The board whose identity and firmware answer the app is holding is
    /// gone (an empty or failed rescan): drop both. Dropping the firmware
    /// check matters as much as clearing the details --- a replug, even on
    /// the very same port, is a board ChipTUI has not asked about, so the
    /// identification re-runs instead of trusting an answer that belonged
    /// to whatever was plugged in before.
    pub(super) fn device_disconnected(&mut self) {
        self.firmware_check_port = None;
        self.firmware_check = FirmwareCheck::Idle;
        // The version banner is as much the departed board's answer as the
        // identity above; a replug must re-answer, not inherit.
        self.mpy_version = None;
        if let Some(flash) = &mut self.flash {
            flash.clear_device_details();
        }
    }

    /// Arms the firmware-identification read for the selected device once
    /// the background chip query has succeeded: reading flash is the only
    /// way to say *which* firmware the board runs, and the read belongs to
    /// the same probe → chip identity → firmware → listing chain the
    /// selection started --- esptool has already reset the board once to
    /// read the chip, so the read adds no interruption the chain has not
    /// already made. Once per port: the answer survives until the
    /// selection changes, the board leaves, or a re-flash invalidates it.
    pub(super) fn arm_firmware_check(&mut self) {
        let Some(port) = self.devices.selected_port().map(str::to_string) else {
            return;
        };
        if self.firmware_check_port.as_deref() == Some(port.as_str()) {
            return;
        }
        // Only a successful identity read says the board answers esptool
        // at all; a failed one leaves the pane at its honest placeholder
        // and the firmware unidentified --- a board the chip query cannot
        // reach (no esptool-backed bootloader) is never gated, and the
        // listing lets mpremote fail on its own instead.
        if !self
            .flash
            .as_ref()
            .is_some_and(|flash| matches!(flash.state, RunState::Succeeded))
        {
            return;
        }
        self.firmware_check_port = Some(port);
        self.firmware_check = FirmwareCheck::Pending;
        if let Some(flash) = &mut self.flash {
            flash.clear_firmware_identity();
        }
    }

    /// The device's flash was just rewritten --- `west flash` from the
    /// build panel, or an esptool write/erase from the flash panel --- so
    /// the firmware the identification read named is as stale as the flash
    /// it was read from. No device listing is coming that would re-ask the
    /// question on its own, so this invalidates the old answer (the same
    /// fields `FlashUpdate::firmware_invalidated` once cleared piecemeal)
    /// and re-arms the read directly --- a *new* identification runs as
    /// soon as the port is free. A board that is not selected, or never
    /// answered the chip query, keeps nothing to re-ask:
    /// `arm_firmware_check` refuses both.
    pub(super) fn reidentify_firmware_after_flash(&mut self) {
        if self.devices.selected_port().is_none() {
            return;
        }
        self.firmware_check_port = None;
        self.firmware_check = FirmwareCheck::Idle;
        if let Some(flash) = self.flash.as_mut() {
            flash.clear_firmware_identity();
        }
        self.mpy_version = None;
        self.probed_port = None;
        self.set_script_state(ScriptState::Unknown);
        self.arm_firmware_check();
        self.logs
            .info("flash changed the device — re-identifying its firmware");
    }

    /// Starts the armed identification read once every port holder is
    /// gone, polled on every tick and after each process event --- the
    /// same guards as the chip query, minus the script belief: by the time
    /// the read is armed the chip query has already reset the board, so
    /// there is no running script left to protect (and an accepted
    /// interruption's restore question waits for this read, so guarding on
    /// it would deadlock). A refusal (a manual command owns the panel, or
    /// the port vanished) concludes the check without a verdict --- the
    /// same courtesy-not-worth-interrupting rule --- and the listing falls
    /// back to letting mpremote speak for itself.
    pub(super) fn maybe_run_deferred_firmware_check(&mut self) -> DeferredQuery {
        if self.firmware_check != FirmwareCheck::Pending
            || self.overlay.is_some()
            || self.probe.is_some()
            || self.device_monitor_process.is_some()
            || self.run_process.is_some()
            || self.browser.as_ref().is_some_and(Browser::is_busy)
            || self.flash_query_pending
        {
            return DeferredQuery::Waiting;
        }
        self.firmware_check = FirmwareCheck::Idle;
        if self.maybe_query_firmware_identity() {
            self.firmware_check = FirmwareCheck::Running;
            DeferredQuery::Started
        } else {
            DeferredQuery::Dropped
        }
    }

    /// Starts the background `esptool read-flash` identification query,
    /// same shape as [`Self::maybe_query_device_info`]. Returns whether it
    /// started.
    fn maybe_query_firmware_identity(&mut self) -> bool {
        if !self.ensure_flash_panel() {
            return false;
        }
        let Some(port) = self.devices.selected_port().map(str::to_string) else {
            return false;
        };
        let Some(mut flash) = self.flash.take() else {
            return false;
        };
        let started = flash.query_firmware_identity(&mut self.processes, Some(&port));
        self.flash = Some(flash);
        started
    }

    /// Runs the version hunt (`FlashPanel::query_firmware_version`) once the
    /// identification read armed it: the follow-up read is pure courtesy ---
    /// nothing waits on it, so it only ever starts when the port is free,
    /// nothing interactive holds the session (an overlay must not sit on top
    /// of an esptool that resets the board underneath it), and the selected
    /// port still exists. A verdict that no longer standing, or a port that
    /// vanished, drops the hunt: a bare firmware name is the honest answer
    /// it already was.
    pub(super) fn maybe_run_deferred_version_hunt(&mut self) {
        let Some(flash) = self.flash.as_ref() else {
            return;
        };
        if !flash.has_pending_version_hunt() {
            return;
        }
        if self.overlay.is_some()
            || self.probe.is_some()
            || self.device_monitor_process.is_some()
            || self.run_process.is_some()
            || self.browser.as_ref().is_some_and(Browser::is_busy)
        {
            return;
        }
        let Some(port) = self.devices.selected_port().map(str::to_string) else {
            if let Some(flash) = self.flash.as_mut() {
                flash.drop_version_hunt();
            }
            return;
        };
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        flash.query_firmware_version(&mut self.processes, Some(&port));
        self.flash = Some(flash);
    }

    /// The firmware half of the first-listing chain: after the chip
    /// identity, the board's firmware decides whether mpremote has
    /// anything to talk to. Only MicroPython exposes a filesystem over
    /// its REPL, so a verdict of Zephyr, ESP-IDF or erased flash refuses
    /// the listing with the reason in the device pane --- instead of
    /// garbage-listing a board that was never going to answer
    /// ([`Self::load_device_root`] holds the listing here, and the chip
    /// query's finish arms the read for a newly selected device).
    pub(super) fn hold_root_listing_for_firmware(&mut self) -> FirmwareHold {
        // The chain's previous link still owns the ordering: while the chip
        // query is pending or running, the listing is held behind *it*, and
        // this gate has no say until the query's finish event arms (or
        // declines) the read. That includes a script believed running ---
        // a board printing a boot banner (any foreign firmware on an
        // auto-reset ESP32) looks exactly like a busy script to the probe,
        // so the listing must not slip past while the chip query politely
        // waits for the belief to clear; the interrupt question asks about
        // the identification instead (`App::check_interrupt_gate`) and only
        // an accepted interruption moves the chain forward.
        if self.flash_query_pending
            || self
                .flash
                .as_ref()
                .is_some_and(FlashPanel::chip_query_running)
        {
            self.held_root_listing = true;
            return FirmwareHold::Held;
        }
        let Some(port) = self.devices.selected_port().map(str::to_string) else {
            return FirmwareHold::Release;
        };
        if self.firmware_check_port.as_deref() != Some(port.as_str()) {
            // Not armed for this port: the chip query never succeeded for
            // it, or the read was refused --- identification has no answer
            // to give, and the listing proceeds to fail or succeed on its
            // own.
            return FirmwareHold::Release;
        }
        if self.firmware_check != FirmwareCheck::Idle {
            // Try to start a pending read now rather than wait a tick,
            // mirroring the chip hold; a guard that still applies keeps it
            // pending for the tick's next poll.
            if self.firmware_check == FirmwareCheck::Pending {
                self.maybe_run_deferred_firmware_check();
            }
            self.held_root_listing = true;
            return FirmwareHold::Held;
        }
        let reason = self
            .flash
            .as_ref()
            .and_then(|flash| flash.details.firmware.clone())
            .and_then(non_micropython_block_reason);
        match reason {
            Some(reason) => {
                // A re-entry (a rescan re-selecting the same board) lands
                // here again; the pane keeps its message either way, but
                // the log should not repeat itself.
                let already_refused = self.browser.as_ref().is_some_and(|browser| {
                    matches!(&browser.device_state, PaneState::Failed(current) if current == &reason)
                });
                self.set_device_pane_error(reason.clone());
                if !already_refused {
                    self.logs.warn(reason);
                }
                FirmwareHold::Blocked
            }
            // MicroPython confirmed, or nothing recognizable: both list.
            None => FirmwareHold::Release,
        }
    }

    /// Re-evaluates a listing held behind the identification chain (chip
    /// query, then firmware read) whenever something it waits on reports
    /// back: the chip query's finish arms the read, the read's finish
    /// applies the verdict, and a query that can never start releases
    /// the listing rather than strand it. A verdict that refuses the
    /// listing drops it with the reason already in the pane.
    pub(super) fn drive_held_root_listing(&mut self) {
        if !self.held_root_listing {
            return;
        }
        match self.hold_root_listing_for_firmware() {
            FirmwareHold::Release => self.resume_held_root_listing(),
            FirmwareHold::Held => {}
            FirmwareHold::Blocked => {
                self.held_root_listing = false;
            }
        }
    }

    /// Holds the first listing of a newly selected device behind the
    /// background `esptool chip-id`: the board's identity is the cheapest
    /// question worth asking a port that was just selected, and asking it
    /// first keeps `esptool`'s board reset from ever landing mid-listing.
    /// [`Self::load_device_root`] calls this right after the probe (if any)
    /// released the port; the listing's next stop is the firmware gate
    /// ([`Self::hold_root_listing_for_firmware`]).
    ///
    /// `false` means the listing should not wait: nothing is pending, the
    /// query can never run for this backend (no esptool-backed capability,
    /// in which case the pending flag is dropped rather than held forever),
    /// a script believed running owns the ordering instead (the listing
    /// queues and the interrupt gate asks), or the query was refused
    /// outright. A held listing is released by [`Self::resume_held_root_listing`].
    pub(super) fn hold_root_listing_for_chip_identity(&mut self) -> bool {
        if !self.flash_query_pending || self.devices.script_state() == ScriptState::Running {
            return false;
        }
        if !self.ensure_flash_panel() {
            self.flash_query_pending = false;
            return false;
        }
        // Try to start the query now rather than wait a tick; a guard that
        // still applies (an open overlay, a busy browser) keeps the listing
        // held, and whichever event lifts the guard leads back here through
        // the tick / process-event paths that also poll the query.
        let outcome = self.maybe_run_deferred_flash_query();
        self.held_root_listing = outcome != DeferredQuery::Dropped;
        self.held_root_listing
    }

    /// Releases a listing held behind the background chip query, listing
    /// the device root for real ([`Self::load_device_root`], which no
    /// longer finds a pending query to wait for). A no-op when nothing is
    /// held --- the common case for the plain courtesy refresh.
    pub(super) fn resume_held_root_listing(&mut self) {
        if !self.held_root_listing {
            return;
        }
        self.held_root_listing = false;
        self.load_device_root();
    }

    /// The user confirmed erasing flash to install MicroPython
    /// (`Overlay::ConfirmEraseForMicroPython`). Same port-contention concern
    /// as [`App::apply_device_picker`]: `esptool` cannot open the serial
    /// port while `mpremote` still holds it, so this defers the query
    /// instead of racing it whenever the browser has a request in flight.
    pub(super) fn confirm_erase_for_micropython(&mut self) {
        self.ensure_flash_panel();
        if let Some(flash) = &mut self.flash {
            flash.screen = crate::flash::FlashScreen::OnlineBoards;
            self.view = View::Flash;
        }
        if self.browser.as_ref().is_some_and(Browser::is_busy) {
            self.defer_device_info_query();
        } else {
            self.maybe_query_device_info();
        }
    }

    pub(super) fn apply_firmware_picker(&mut self, selected: usize) {
        let Some(flash) = &mut self.flash else {
            return;
        };
        if !flash.select_firmware(selected) {
            return;
        }
        flash.screen = FlashScreen::Options;
        flash.options_focus = OptionsField::Chip;
    }

    /// An erase just succeeded and firmware discovery already ran
    /// (`FlashPanel::on_process`). Never flashes on its own --- that still
    /// needs its own confirmation --- only decides whether to ask which file
    /// or go straight to the options screen for the one that was found.
    ///
    /// The erase itself already moved the user to the Monitor tab
    /// (`show_flash_in_monitor`), so when there is something to offer, this
    /// reopens the Flash dialog --- the write-flash step needs the user
    /// looking at it, not left on a tab they may not be watching.
    pub(super) fn offer_flash_after_erase(&mut self) {
        let Some(flash) = &mut self.flash else {
            return;
        };
        flash.set_cursor_to(FlashAction::WriteFlash);
        flash.set_pane_cursor_to(FlashAction::WriteFlash);
        match flash.firmware.len() {
            0 => {}
            1 => {
                flash.screen = FlashScreen::Options;
                self.view = View::Flash;
            }
            _ => {
                self.overlay = Some(Overlay::FirmwarePicker { selected: 0 });
                self.view = View::Flash;
            }
        }
    }

    /// A firmware file just finished downloading. Picks it up as an ordinary
    /// local candidate and hands off to the exact same confirmed
    /// `erase_flash` → `write_flash` chain the Flash menu already uses
    /// (`SPEC.md` §9) --- its confirm overlay, showing the literal command,
    /// *is* "ask the user whether to erase_flash and flash"; no separate
    /// combined action or confirmation type is needed.
    pub(super) fn offer_flash_after_download(&mut self) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let notices = flash.discover_firmware();
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
        self.trigger_flash_action(FlashAction::EraseFlash);
    }
}

/// The message a non-MicroPython firmware verdict refuses the file listing
/// with: files can only be read on MicroPython, so the pane must say that
/// --- and name the firmware (and version, when the read found one) that
/// answered instead --- rather than show mpremote's failure to talk to a
/// firmware it does not speak. `None` for MicroPython (no reason to
/// refuse).
fn non_micropython_block_reason(verdict: FirmwareVerdict) -> Option<String> {
    match verdict {
        FirmwareVerdict::Firmware(FlashFirmware::MicroPython, _) => None,
        FirmwareVerdict::Firmware(other, version) => {
            let named = match version {
                Some(version) => format!("{} {version}", other.label()),
                None => other.label().to_string(),
            };
            Some(format!(
                "cannot read files — the device runs {named}, not MicroPython"
            ))
        }
        FirmwareVerdict::Erased => {
            Some("no firmware on the device — flash MicroPython to browse its files".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_non_micropython_verdicts_refuse_the_listing() {
        assert_eq!(
            non_micropython_block_reason(FirmwareVerdict::Firmware(
                FlashFirmware::MicroPython,
                Some("v1.28.0".to_string())
            )),
            None,
            "MicroPython is the one verdict the listing may proceed on"
        );
        for (verdict, needle) in [
            (
                FirmwareVerdict::Firmware(FlashFirmware::Zephyr, Some("v4.0.0".to_string())),
                "Zephyr v4.0.0",
            ),
            (
                FirmwareVerdict::Firmware(FlashFirmware::EspIdf, None),
                "ESP-IDF",
            ),
        ] {
            let reason =
                non_micropython_block_reason(verdict).expect("a foreign firmware must refuse");
            assert!(
                reason.contains("cannot read files") && reason.contains(needle),
                "the reason must say what is refused and by what: {reason}"
            );
        }
        let erased = non_micropython_block_reason(FirmwareVerdict::Erased)
            .expect("a blank chip has no files to list");
        assert!(
            erased.contains("no firmware") && erased.contains("flash MicroPython"),
            "an erased flash must point at the way out: {erased}"
        );
    }
}
