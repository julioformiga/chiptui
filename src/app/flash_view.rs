//! `esptool` flash-view dispatch: the menu, options screen, online
//! board/firmware search, and everything that decides when a destructive
//! action needs confirmation. Split out of `app.rs` since it is the one
//! subsystem `App` drives almost entirely through [`crate::flash::FlashPanel`]
//! and never touches [`crate::browser`] directly (beyond checking whether it
//! still holds the serial port).

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::Capability;
use crate::browser::Browser;
use crate::device::ScriptState;
use crate::flash::{FlashAction, FlashPanel, FlashScreen, OptionsField};

use super::{App, Focus, LogTab, MonitorSource, Overlay, View};

/// What trying to start the deferred device query concluded, so callers
/// that chained something behind it (the first device listing,
/// [`App::hold_root_listing_for_chip_identity`]) know whether to keep
/// waiting or move on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeferredQuery {
    /// A guard still holds it (an open overlay, a busy port holder, a
    /// script believed running); it stays pending and will be retried.
    Waiting,
    /// The query is running; a `background_query_finished` will follow.
    Started,
    /// The query was consumed but could not start (a manual esptool
    /// command owns the panel, or the port vanished) --- nothing will
    /// follow it.
    Dropped,
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
                    | FlashScreen::CustomUrl => {
                        if let Some(flash) = &mut self.flash {
                            flash.screen = FlashScreen::Menu;
                        }
                    }
                }
                return;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.overlay = Some(Overlay::Help { selected: 0 });
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
    /// (`SPEC.md` §9), narrowed by the selected device's board vendor when
    /// its vid:pid identifies one (`DeviceInfo::board_vendor`).
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
                .warn("no chip known yet --- run 'chip information' or pick one in Options first");
            return;
        };
        let vendor = self
            .devices
            .selected()
            .and_then(|device| device.board_vendor());
        let notices = flash.search_online(mcu, vendor, &mut self.processes);
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
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
                0 => {}
                1 => {
                    flash.screen = FlashScreen::Options;
                    flash.options_focus = OptionsField::Chip;
                }
                _ => self.overlay = Some(Overlay::FirmwarePicker { selected: 0 }),
            }
            self.flash = Some(flash);
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

    /// Closes the Flash dialog and moves focus to the Monitor tab, which
    /// streams [`FlashPanel::output`] as it arrives --- called after a flash
    /// action successfully spawns, replacing the dialog's former
    /// `FlashScreen::Output` screen (`SPEC.md` §11).
    pub(super) fn show_flash_in_monitor(&mut self) {
        self.view = View::Dashboard;
        self.focus = Focus::Logs;
        self.log_tab = LogTab::Monitor;
        self.set_monitor_source(MonitorSource::Flash);
    }

    /// Opens the flash view, if the selected backend can flash or erase.
    ///
    /// The gate is the capability, never the backend kind (`AGENTS.md` §3).
    /// A backend whose flashing lives in the build panel (`west flash`
    /// behind a confirm, e.g. Zephyr) is routed there instead of esptool's
    /// dialog --- this view is esptool-specific.
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
        self.view = View::Flash;
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
    /// so `mpremote devs` → probe → chip-id → listing is the one order in
    /// which the port changes hands cleanly. `maybe_run_deferred_flash_query`
    /// starts the query for real once every port holder is gone.
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
    pub(super) fn maybe_run_deferred_flash_query(&mut self) -> DeferredQuery {
        if !self.flash_query_pending
            || self.overlay.is_some()
            || self.restore_pending
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

    /// Holds the first listing of a newly selected device behind the
    /// background `esptool chip-id`: the board's identity is the cheapest
    /// question worth asking a port that was just selected, and asking it
    /// first keeps `esptool`'s board reset from ever landing mid-listing.
    /// [`Self::load_device_root`] calls this right after the probe (if any)
    /// released the port.
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
        flash.screen = FlashScreen::Menu;
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
        self.trigger_flash_action(FlashAction::EraseFlash);
    }
}
