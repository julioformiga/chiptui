//! The keyboard's front door: [`App::on_key`] and the dashboard dispatch
//! beneath it.
//!
//! Order matters here more than anywhere else in the app. A live Terminal
//! tab owns the keyboard outright (so `ctrl+c` reaches the shell's job, not
//! the quit path); the dashboard-wide chords (`ctrl+←/→`, `ctrl+↑/↓`,
//! `ctrl+r`, `m`, `s`, the pane digits) are intercepted *before* the focused
//! pane sees anything, which is what keeps a chord from leaking into a
//! pane's own arrows; and only then does the key reach the pane the cursor
//! sits in.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::backend::Capability;

use super::{
    App, Focus, LogTab, MonitorScroll, OVERLAY_HELP, Overlay, View, key_to_bytes, terminal,
};

impl App {
    pub(super) fn on_key(&mut self, key: KeyEvent) {
        // `ctrl+f`: the row 3 fullscreen toggle. Checked ahead of the
        // monitor/terminal keyboard capture below (both `return` before
        // reaching `on_dashboard_key`'s own chords) so it works exactly
        // where it is most wanted --- full width on a live monitor or
        // shell session, not only from the dashboard's other panes.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
        {
            self.toggle_row3_fullscreen();
            return;
        }
        if self.is_monitor_active() {
            // `ctrl+]` is ChipTUI's chord on this tab --- the session stops
            // here, deterministically (crossterm relabels the byte Ctrl+5).
            // The child's own exit key is the child's business and cannot be
            // relied on: the idf_monitor `west espressif monitor` runs (the
            // 1.1 vendored in hal_espressif) hangs on *any* exit key on
            // kernels without TIOCSTI (>= 6.2) --- its stop path unblocks the
            // blocked key read by injecting a byte with TIOCSTI, then joins
            // the reader thread, and the removed ioctl leaves that join
            // stuck forever. Stopping from here (SIGTERM to the child's
            // group) ends any monitor, mpremote's included, and releases the
            // port. The child's other keys still forward untouched, so the
            // documented in-tool exits (idf_monitor's Ctrl+T menu) keep
            // working where the child implements them.
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
            {
                if let Some(id) = self.device_monitor_process {
                    self.logs.info("stopping monitor (ctrl+])");
                    self.processes.cancel(id);
                }
                return;
            }
            if let Some((id, bytes)) = self.device_monitor_process.zip(key_to_bytes(key)) {
                self.processes.write_stdin(id, &bytes);
            }
            return;
        }

        // The Terminal tab's shell owns the keyboard exactly like the device
        // monitor does: every keystroke becomes bytes in its PTY. One
        // escape, and it is the monitor's own chord --- `ctrl+]`, the byte
        // crossterm relabels Ctrl+5. For mpremote it is the *exit* key; a
        // shell has no use for 0x1d, so the same chord becomes *detach*
        // instead: the shell keeps running (and streaming into the tab)
        // while the keyboard returns to the dashboard.
        if self.is_terminal_active() {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
            {
                self.terminal_detached = true;
                return;
            }
            // `shift+pgup`/`shift+pgdn` reach the scrollback, because plain
            // PageUp now belongs to the shell like every other key. This is
            // the same division a real terminal makes: the emulator keeps
            // the shifted pair for its own history and never forwards it.
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            {
                let page = self.page();
                if key.code == KeyCode::PageUp {
                    self.monitor_scroll_up(page);
                } else {
                    self.monitor_scroll_down(page);
                }
                return;
            }
            let application_cursor = self.terminal.screen().application_cursor();
            if let Some((id, bytes)) = self
                .terminal_process
                .zip(terminal::terminal_key_bytes(key, application_cursor))
            {
                // Typing snaps back to the live screen, the way every
                // terminal does: the output the keystroke provokes is the
                // output the user wants to see.
                self.monitor_scroll = MonitorScroll::default();
                self.processes.write_stdin(id, &bytes);
            }
            return;
        }

        // Ctrl+C during an active run sends a KeyboardInterrupt (0x03) to the
        // device script instead of quitting the TUI.
        if self.is_run_active()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c'))
        {
            if let Some(id) = self.run_process {
                self.processes.write_stdin(id, &[0x03]);
            }
            return;
        }

        // Raw mode swallows SIGINT, so Ctrl+C has to be handled explicitly and
        // must work regardless of focus or overlay.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit();
            return;
        }

        if self.overlay.is_some() {
            self.on_overlay_key(key);
            return;
        }

        match self.view {
            View::Dashboard => self.on_dashboard_key(key),
            View::Flash => self.on_flash_key(key),
        }
    }

    /// Keys while the Device Info pane holds focus. The pane is a report
    /// with one actionable row --- the MAC, selected the moment focus
    /// arrives (`ui::panels::draw_detection`) --- so `Enter` copies it the
    /// same way the row's click does, and nothing else is bound.
    pub(super) fn on_device_info_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Enter {
            return;
        }
        // The pane's empty state is one gesture from being answered: the
        // identification question the pane's message names (`ctrl+r`'s
        // in-pane twin). Only when there is nothing to show --- with a MAC
        // read, `Enter` keeps its copy meaning.
        if self
            .flash
            .as_ref()
            .is_none_or(|flash| flash.details.is_empty())
        {
            self.open_identification_question();
            return;
        }
        if let Some(mac) = self
            .flash
            .as_ref()
            .and_then(|flash| flash.details.mac.clone())
        {
            self.copy_to_clipboard("MAC", mac);
        }
    }

    pub(super) fn on_dashboard_key(&mut self, key: KeyEvent) {
        if self.handle_shortcuts_overlay_key(key) {
            return;
        }

        match key.code {
            // `q` only. A reflex `Esc` ("close what is open") must not end
            // the session: with no overlay open it is a no-op here --- every
            // overlay handles its own `Esc` before this point, and the Flash
            // view keeps `Esc` as "back one screen".
            KeyCode::Char('q') => {
                self.quit();
                return;
            }
            // A no-op while row 3 is fullscreen: every other pane is
            // undrawn, so there is nowhere else to step focus to.
            KeyCode::Tab if !self.row3_fullscreen => {
                self.step_focus(true);
                return;
            }
            KeyCode::BackTab if !self.row3_fullscreen => {
                self.step_focus(false);
                return;
            }
            // The ctrl-arrow chord is dashboard-wide, and spends itself on
            // whatever the focused pane owns: its tab strip when it has one
            // (row 3; a flash-capable device pane), the pane *beside* it in
            // the same row otherwise (Environment ↔ Device Info, the local
            // column ↔ the device pane, workspace ↔ build --- see
            // `switch_strip_tabs`/`step_focus_horizontal`). Placed before
            // the focus dispatch (like `m` and `s`), it also keeps the
            // chord out of the panes' own arrow grammars: on the local
            // files pane it must never descend a directory, in row 3 it
            // merely joins the plain arrows nothing competes with there.
            KeyCode::Left | KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.switch_strip_tabs(key.code == KeyCode::Right);
                return;
            }
            // The vertical half of the same chord family: ctrl+↓/ctrl+↑
            // steps focus between the dashboard's *rows* of panes, the
            // geometry `Tab`'s linear tour flattens. Intercepted beside the
            // strip chord so the plain arrows keep belonging to the focused
            // pane's own grammar --- on a file column they descend and
            // ascend, they never leave the pane. A no-op while row 3 is
            // fullscreen, like `Tab`: every other row is undrawn, so there
            // is nowhere to step to.
            KeyCode::Up | KeyCode::Down
                if !self.row3_fullscreen && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.step_focus_vertical(key.code == KeyCode::Down);
                return;
            }
            // `1`..`5` jump to the numbered panes --- the numbers every
            // pane's title carries (`ui::numbered_title`), fixed per
            // position rather than per tour stop, so they are the same in
            // every backend and worth memorizing. A digit with no focusable
            // pane behind it falls through to the focused pane, keeping the
            // digits available to future pane grammars.
            KeyCode::Char(digit @ '1'..='9') if !self.row3_fullscreen => {
                if let Some(stop) = self.pane_for_number(digit as u8 - b'0') {
                    self.focus = stop;
                    return;
                }
            }
            KeyCode::Char('t') => {
                self.open_theme_picker();
                return;
            }
            // The icon cycle: `unicode` → `nerd` → `none`, applied and
            // persisted. The CONTROL guard is what keeps the plain `i` of
            // the device pane's package install falling through to it, and
            // the guard itself only ever passes on a Kitty-protocol
            // terminal --- legacy sends Ctrl+I as Tab, which the arm above
            // keeps as the focus tour.
            KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_icon_set();
                return;
            }
            KeyCode::Char('x') => {
                self.open_flash();
                return;
            }
            KeyCode::Char('P') => {
                self.request_home_screen();
                return;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.overlay = Some(OVERLAY_HELP);
                return;
            }
            KeyCode::Char('d') => {
                // `d` re-runs whichever discovery this backend has: the
                // mpremote listing under `Filesystem`, else (a monitor
                // backend with no filesystem, e.g. Zephyr) the plain USB
                // serial walk.
                if self.manager.capabilities().contains(Capability::Filesystem) {
                    self.scan_devices();
                } else if self.manager.capabilities().contains(Capability::Monitor) {
                    self.scan_serial_devices();
                }
                return;
            }
            KeyCode::Char('m') if self.manager.capabilities().contains(Capability::Monitor) => {
                self.open_monitor();
                return;
            }
            // The explicit "stop the device and capture its data" gesture,
            // from any pane (`Enter` on a Device info pane with nothing to
            // show is its in-pane twin): reading the chip and firmware
            // restarts the board, so the gesture opens the same question a
            // device selection does ([`Overlay::ConfirmIdentifyDevice`],
            // default No) rather than firing the chain outright. The
            // CONTROL guard is what keeps the pane-local plain `r` (reload,
            // rename, re-detect) falling through to its own grammar.
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_identification_question();
                return;
            }
            // The SDK's toolchains, without walking the whole installer
            // flow to reach them: adding one to an existing SDK is a
            // routine errand (a new board needs a target the bundle was
            // not unpacked with), and it used to cost five keystrokes of
            // navigation through questions that were already answered.
            //
            // Placed here, before the focus dispatch, so it works from
            // whichever pane holds the cursor --- like `m`. The
            // MicroPython `s` (save a captured run's output) is guarded by
            // `is_run_view` further down and belongs to a backend without
            // this capability, so the two never both apply.
            KeyCode::Char('s')
                if self
                    .manager
                    .capabilities()
                    .contains(Capability::WorkspaceSync) =>
            {
                self.open_sdk_toolchains_shortcut();
                return;
            }
            // Capital so it never collides with plain `r` (re-detect / reload
            // pane) --- a restart interrupts whatever the board is doing, so
            // it goes through the same confirm-first dialog as the
            // post-edit-reupload prompt rather than firing immediately.
            KeyCode::Char('R') if self.manager.capabilities().contains(Capability::Reset) => {
                self.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });
                return;
            }
            _ => {}
        }

        // The device pane's Actions tab: its own grammar (buttons,
        // not a listing), so it takes the keys before the browser does.
        // Its stacked buttons take `↑/↓` alone; the plain `←/→` are owned
        // by nothing there --- tabs answer to the ctrl chord only, from
        // any pane (one grammar for every strip).
        if self.focus == Focus::FilesDevice && self.device_actions_tab_active() {
            self.on_flash_pane_key(key);
            return;
        }

        if matches!(self.focus, Focus::FilesLocal | Focus::FilesDevice)
            && !self.build_pane_visible_precondition()
        {
            self.on_files_key(key);
            return;
        }

        if self.focus == Focus::Project {
            self.on_project_key(key);
            return;
        }

        // Device Info: one actionable row, the MAC. Everything else is
        // read-only report, so no key but `Enter` (and the dashboard-wide
        // arms above) does anything here.
        if self.focus == Focus::DeviceInfo {
            self.on_device_info_key(key);
            return;
        }

        if self.focus == Focus::Workspace {
            self.on_workspace_key(key);
            return;
        }

        if self.focus == Focus::Build {
            self.on_build_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('s') if self.is_run_view() => {
                self.save_run_output();
            }
            // On the Terminal tab with no shell alive, `r` starts another
            // without leaving the tab --- the recovery from a spawn failure
            // (or from having exited the shell and wanting it back).
            KeyCode::Char('r')
                if self.focus == Focus::Logs
                    && self.log_tab == LogTab::Terminal
                    && self.terminal_process.is_none() =>
            {
                self.start_terminal_shell();
            }
            KeyCode::Char('r') => {
                self.logs.info("re-running project detection");
                self.detect();
                self.maybe_open_project_setup();
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.page() as isize)),
            KeyCode::PageDown => self.move_cursor(self.page() as isize),
            KeyCode::Home => self.jump_to_start(),
            KeyCode::End => self.jump_to_end(),
            _ => {}
        }
    }

    /// Page size for the focused pane.
    pub(super) fn page(&self) -> usize {
        match self.focus {
            Focus::Logs if self.log_tab != LogTab::Log => self.monitor_view.viewport.max(1),
            Focus::Logs => self.log_viewport.max(1),
            _ => 5,
        }
    }

    pub(super) fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            // The Monitor tab tails live output only while following; once
            // the user scrolls it holds its top-anchored position (new
            // output grows the document below the view).
            Focus::Logs if self.log_tab == LogTab::Log => {
                // The log pane scrolls; up means "towards older entries".
                if delta < 0 {
                    self.logs.scroll_up(delta.unsigned_abs(), self.log_viewport);
                } else {
                    self.logs.scroll_down(delta as usize);
                }
            }
            Focus::Logs if self.log_tab != LogTab::Log => {
                if delta < 0 {
                    self.monitor_scroll_up(delta.unsigned_abs());
                } else {
                    self.monitor_scroll_down(delta as usize);
                }
            }
            // FilesLocal/FilesDevice/Workspace/Build/Project never reach
            // here: on_dashboard_key routes them to their own key handlers
            // first.
            _ => {}
        }
    }

    pub(super) fn jump_to_start(&mut self) {
        match self.focus {
            Focus::Logs if self.log_tab == LogTab::Log => {
                self.logs.scroll_up(usize::MAX, self.log_viewport);
            }
            Focus::Logs if self.log_tab != LogTab::Log => {
                self.monitor_scroll.following = false;
                self.monitor_scroll.offset = 0;
            }
            _ => {}
        }
    }

    pub(super) fn jump_to_end(&mut self) {
        match self.focus {
            Focus::Logs if self.log_tab == LogTab::Log => self.logs.scroll_to_bottom(),
            Focus::Logs if self.log_tab != LogTab::Log => {
                self.monitor_scroll = MonitorScroll::default();
            }
            _ => {}
        }
    }
}
