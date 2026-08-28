//! The build dashboard window's keyboard and its door.
//!
//! The state and every derivation live in [`crate::build_dashboard`]; this
//! is the `impl App` half --- opening the window over the right build
//! directory, and the key grammar.
//!
//! That grammar is [`super::packages`]' (which is the board/shield pickers'),
//! and its governing rule is the same: **every printable character is filter
//! text**. No action may live on a plain letter, which is why `q` does not
//! close the window (only `Esc` does), why there is no reload key, and why
//! the tab strip answers `ctrl+←/→` --- the dashboard-wide chord --- rather
//! than the plain arrows, which the list, the details and the two trees
//! already need.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::backend::zephyr::report::ReportPaths;
use crate::build_dashboard::DashboardTab;

use super::{App, DocsFocus, Overlay};

impl App {
    /// Opens the window over the build panel's current project and build
    /// directory, loading whatever the tab in view needs.
    ///
    /// Reached from the Zephyr Actions menu. No dashboard-wide letter opens
    /// it: this is a Zephyr-only surface two keystrokes from the pane that
    /// owns it, and spending a global letter on it would cost another
    /// backend one (adding `s` for the SDK picker already forced
    /// MicroPython's `s` to declare a capability it never had).
    pub(crate) fn open_build_dashboard(&mut self) {
        let Some(paths) = self.build_report_paths() else {
            self.logs
                .warn("no build directory --- configure a project first");
            return;
        };
        self.build_dashboard.ensure_tab(&paths);
        self.dashboard_list_offset = 0;
        self.overlay = Some(Overlay::BuildDashboard);
    }

    /// Where the artifacts live, for the project the build panel targets.
    /// `None` before a project resolves, which is also when there is
    /// nothing to report on.
    pub(crate) fn build_report_paths(&mut self) -> Option<ReportPaths> {
        let (root, build_dir) = {
            let panel = self.build.as_ref()?;
            (panel.root.clone(), panel.build_dir.clone())
        };
        // Pointing at another build clears every cache, so one project's
        // symbols can never be shown under another's name.
        self.build_dashboard.retarget(&root, &build_dir);
        Some(ReportPaths::new(&root, &build_dir))
    }

    /// Switches tab and loads what the new one needs.
    pub(crate) fn set_dashboard_tab(&mut self, tab: DashboardTab) {
        if self.build_dashboard.tab == tab {
            return;
        }
        self.build_dashboard.tab = tab;
        self.dashboard_list_offset = 0;
        if let Some(paths) = self.build_report_paths() {
            self.build_dashboard.ensure_tab(&paths);
        }
    }

    /// Walks the strip in the arrow's direction, clamped --- never wrapping,
    /// the rule every other strip in the app follows.
    pub(crate) fn step_dashboard_tab(&mut self, steps: i32) {
        self.set_dashboard_tab(self.build_dashboard.tab.stepped(steps));
    }

    /// Hands the keyboard to one half of the body. The mouse's `Tab`.
    pub(crate) fn set_dashboard_focus(&mut self, focus: DocsFocus) {
        self.build_dashboard.focus = focus;
    }

    /// Selects a row without activating it --- the picker grammar every
    /// click in this app follows.
    pub(crate) fn set_dashboard_selection(&mut self, index: usize) {
        self.build_dashboard.pane_mut().selected = index;
    }

    /// The strip's `(tab, title)` pairs in draw order.
    ///
    /// One definition, consumed by the renderer *and* by the click
    /// hit-testing (`mouse::strip_tab` walks these widths), which is the
    /// contract `log_strip_tabs` already keeps for row 3. The window's mark
    /// rides the first title so it is budgeted on both sides.
    pub(crate) fn dashboard_strip_tabs(&self) -> Vec<(DashboardTab, String)> {
        let glyph = self.icon_set().dashboard();
        DashboardTab::ALL
            .into_iter()
            .enumerate()
            .map(|(position, tab)| {
                let title = match (position == 0, glyph.is_empty()) {
                    (true, false) => format!("{glyph} {}", tab.label()),
                    _ => tab.label().to_string(),
                };
                (tab, title)
            })
            .collect()
    }

    pub(super) fn on_build_dashboard_key(&mut self, key: KeyEvent) {
        let details = self.build_dashboard.focus == DocsFocus::Details;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // `q` is filter text, so `Esc` is the only way out --- the same
            // trade the package manager makes for the same reason.
            KeyCode::Esc => self.overlay = None,
            KeyCode::Tab => {
                self.build_dashboard.focus = self.build_dashboard.focus.toggled();
            }

            // The strip answers the chord alone, on every strip in this app.
            KeyCode::Left if control => self.step_dashboard_tab(-1),
            KeyCode::Right if control => self.step_dashboard_tab(1),

            KeyCode::Backspace => {
                self.build_dashboard.pane_mut().input.pop();
                self.reset_dashboard_list();
            }

            // With the details focused the arrows scroll it; otherwise they
            // walk the list, and on a tree the horizontal pair expands.
            KeyCode::Up if details => {
                let pane = self.build_dashboard.pane_mut();
                pane.scroll = pane.scroll.saturating_sub(1);
            }
            KeyCode::Down if details => self.build_dashboard.pane_mut().scroll += 1,
            KeyCode::PageUp if details => {
                let page = self.dashboard_viewport.max(1) as u16;
                let pane = self.build_dashboard.pane_mut();
                pane.scroll = pane.scroll.saturating_sub(page);
            }
            KeyCode::PageDown if details => {
                let page = self.dashboard_viewport.max(1) as u16;
                self.build_dashboard.pane_mut().scroll += page;
            }

            KeyCode::Up => self.move_dashboard_cursor(-1),
            KeyCode::Down => self.move_dashboard_cursor(1),
            KeyCode::PageUp => self.move_dashboard_cursor(-5),
            KeyCode::PageDown => self.move_dashboard_cursor(5),
            KeyCode::Home => self.move_dashboard_cursor(i32::MIN),
            KeyCode::End => self.move_dashboard_cursor(i32::MAX),
            KeyCode::Right => self.build_dashboard.expand_selected(),
            KeyCode::Left => self.build_dashboard.collapse_selected(),
            // The one row in this window that runs something; everywhere
            // else `Enter` opens or closes a tree row, and is a no-op on a
            // flat tab.
            KeyCode::Enter if self.build_dashboard.selected_is_prompt() => {
                self.generate_memory_report();
            }
            KeyCode::Enter => {
                self.build_dashboard.toggle_selected();
            }

            // Every other printable character filters --- including the ones
            // a Ctrl chord would otherwise smuggle in as text.
            KeyCode::Char(c) if !control => {
                self.build_dashboard.pane_mut().input.push(c);
                self.reset_dashboard_list();
            }
            _ => {}
        }
    }

    fn move_dashboard_cursor(&mut self, by: i32) {
        self.build_dashboard.move_cursor(by);
    }

    /// A different list starts from its top, with the details unscrolled ---
    /// the docs pickers' own rule after a filter keystroke.
    fn reset_dashboard_list(&mut self) {
        let pane = self.build_dashboard.pane_mut();
        pane.selected = 0;
        pane.scroll = 0;
        self.dashboard_list_offset = 0;
        self.build_dashboard.clamp_cursor();
    }
}

impl App {
    /// Runs the memory report, closing the window first.
    ///
    /// A command of minutes belongs in the Monitor with `Stop` reachable,
    /// not behind a modal --- the rule the Zephyr Actions menu already
    /// follows for the HTML report. The window comes back through
    /// [`Self::reopen_dashboard_on_memory`] when the run succeeds.
    pub(crate) fn generate_memory_report(&mut self) {
        self.overlay = None;
        self.run_build_action(crate::build::BuildAction::SizeReport);
    }

    /// Re-opens the window on the Memory tab with the new report loaded.
    ///
    /// Only ever called after a *successful* run: on a failure the Monitor
    /// holds the explanation, and a modal over it would hide exactly what
    /// the reader needs.
    pub(crate) fn reopen_dashboard_on_memory(&mut self) {
        self.build_dashboard.tab = DashboardTab::Memory;
        self.build_dashboard.invalidate_memory();
        self.dashboard_list_offset = 0;
        if let Some(paths) = self.build_report_paths() {
            self.build_dashboard.ensure_tab(&paths);
        }
        self.overlay = Some(Overlay::BuildDashboard);
    }
}
