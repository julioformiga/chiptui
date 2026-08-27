//! Where the cursor is, and every way it moves between panes.
//!
//! Three grammars share this module because they answer the same question
//! from different directions: `Tab` walks the working panes in order,
//! `ctrl+←/→` and `ctrl+↑/↓` step by geometry (staying in the same half of
//! the screen), and `1`..`5` jump straight to a pane by the number its
//! title carries. Which panes exist at all is capability-driven, so the
//! tour, the chords and the digits all derive from the same
//! `focusable_in_row`/`focus_order` pair rather than each listing panes of
//! their own.

use crate::backend::Capability;

use super::{App, DevicePaneTab, Focus, LogTab};

impl App {
    /// Whether row 2's right half shows the build panel: the backend can
    /// build, and there is no device filesystem pane to show there instead.
    /// A backend with both would need a third stop --- none exists today
    /// (MicroPython has no build, Zephyr no filesystem), and the capability
    /// pair is the gate, never the backend kind (`AGENTS.md` §3).
    pub fn build_pane_visible(&self) -> bool {
        self.build.is_some() && self.build_pane_visible_precondition()
    }

    /// Whether row 2's left half shows the workspace pane: the backend
    /// maintains a shared environment ([`Capability::WorkspaceSync`]) and
    /// has no device filesystem --- the same pair of conditions that give it
    /// the build panel. Capability-gated, never backend-kind-gated
    /// (`AGENTS.md` §3).
    pub fn workspace_pane_visible(&self) -> bool {
        let caps = self.manager.capabilities();
        self.workspace.is_some()
            && caps.contains(Capability::WorkspaceSync)
            && !caps.contains(Capability::Filesystem)
    }

    /// Whether the device pane can host the **Project actions** tab: the
    /// backend browses a filesystem (the pane exists) *and* can flash or
    /// erase (there are actions to show). The tab strip is drawn whenever
    /// this holds; `x` creates the flash panel and switches to it.
    pub fn device_actions_tab_available(&self) -> bool {
        let caps = self.manager.capabilities();
        self.browser.is_some()
            && caps.contains(Capability::Filesystem)
            && (caps.contains(Capability::Flash) || caps.contains(Capability::EraseFlash))
    }

    /// Whether the device pane is *currently showing* the Project actions
    /// tab --- the state `x` and the pane's arrow keys switch, and the flag
    /// the renderer and the key dispatch branch on.
    pub fn device_actions_tab_active(&self) -> bool {
        self.device_pane_tab == DevicePaneTab::Actions && self.device_actions_tab_available()
    }

    /// `ctrl+f`: row 3 (Log/Monitor/Terminal) takes over the whole
    /// dashboard body, or gives it back. Turning it on parks focus on
    /// [`Focus::Logs`] so the pane's own keys (scrolling, the tab strip,
    /// the terminal/monitor keyboard capture) work the instant the toggle
    /// lands, with nowhere else visible to have been focused on anyway.
    pub(super) fn toggle_row3_fullscreen(&mut self) {
        self.row3_fullscreen = !self.row3_fullscreen;
        if self.row3_fullscreen {
            self.focus = Focus::Logs;
        }
    }

    /// The `ctrl+←/→` chord, live from every pane, spent on whatever the
    /// The `ctrl+←/→` chord, live from every pane, spent on what the
    /// focused pane's geometry offers: row 3 spans the whole width with no
    /// pane beside it, so its chord switches its own strip; every other
    /// pane --- strip or no strip --- walks to the stop beside it, tabs
    /// included (`step_focus_horizontal` counts a tabbed device pane's
    /// strip as stops of its own).
    pub(super) fn switch_strip_tabs(&mut self, forward: bool) {
        match self.focus {
            // Row 3 is its own strip: no sibling pane exists to step to.
            Focus::Logs => self.switch_log_tab(forward),
            _ => self.step_focus_horizontal(forward),
        }
    }

    /// The chord's horizontal half: walk to the stop beside the focused
    /// one *in the arrow's direction*. A row holds exactly two panes
    /// (Environment ↔ Device Info, the local column ↔ the device pane, the
    /// workspace pane ↔ the build panel), and a strip-owning device pane
    /// contributes its tabs as stops of its own, in strip order ---
    /// MicroPython's working row walks pane 3 → pane 4 Actions → pane 4
    /// Device Files, and back the same way, one keypress per stop. The
    /// outermost stop in either direction is the end of the walk: a no-op,
    /// never a wrap. A pane that is not focusable right now ([`Self::
    /// pane_number`]'s visibility rules) ends the walk the same way rather
    /// than receiving a jump onto an undrawn pane.
    pub(super) fn step_focus_horizontal(&mut self, right: bool) {
        if self.row3_fullscreen {
            // The only visible pane is Logs, which owns a strip --- this is
            // reachable solely from a pane the fullscreen row undrew, so
            // there is nothing beside the cursor to step to.
            return;
        }
        // The strip's stops, walked only while the pane already holds the
        // cursor: arriving from outside lands on the first one below.
        if self.focus == Focus::FilesDevice && self.device_actions_tab_available() {
            match (right, self.device_pane_tab) {
                // Actions is the strip's first tab --- the stop nearest
                // pane 3; Device Files is the rightmost stop of all.
                (true, DevicePaneTab::Actions) => {
                    self.device_pane_tab = DevicePaneTab::Files;
                    return;
                }
                (false, DevicePaneTab::Files) => {
                    // The way in creates the flash panel the tab draws,
                    // exactly as `x` does --- the walk is a second door to
                    // the tab, not a lighter one.
                    self.show_device_actions_tab();
                    return;
                }
                // Device Files is the row's right end; Actions is one step
                // from pane 3, and that step is the sibling walk below.
                (true, DevicePaneTab::Files) => return,
                (false, DevicePaneTab::Actions) => {}
            }
        }
        // Each row holds exactly two panes: stepping right is possible only
        // from the left one, stepping left only from the right one.
        let can_step = match self.focus {
            Focus::Project | Focus::FilesLocal | Focus::Workspace => right,
            Focus::DeviceInfo | Focus::FilesDevice | Focus::Build => !right,
            Focus::Logs => false,
        };
        if !can_step {
            return;
        }
        let sibling = match self.focus {
            Focus::Project => Some(Focus::DeviceInfo),
            Focus::DeviceInfo => Some(Focus::Project),
            Focus::FilesLocal => Some(Focus::FilesDevice),
            Focus::FilesDevice => Some(Focus::FilesLocal),
            Focus::Workspace => Some(Focus::Build),
            Focus::Build => Some(Focus::Workspace),
            Focus::Logs => None,
        };
        if let Some(sibling) = sibling
            && self.pane_number(sibling).is_some()
        {
            self.focus = sibling;
            // Arriving at the tabbed device pane from pane 3 lands on the
            // strip's first stop (Actions): the walk enters at its left
            // edge, the way the strip is drawn.
            if sibling == Focus::FilesDevice && self.device_actions_tab_available() {
                self.show_device_actions_tab();
            }
        }
    }

    /// The tabs row 3 offers: `Log` always, `Monitor` when the backend can
    /// monitor, `Terminal` always (a local shell is not a backend
    /// capability, so nothing gates it).
    pub(super) fn available_log_tabs(&self) -> Vec<LogTab> {
        let mut tabs = vec![LogTab::Log];
        if self.manager.capabilities().contains(Capability::Monitor) {
            tabs.push(LogTab::Monitor);
        }
        tabs.push(LogTab::Terminal);
        tabs
    }

    /// Steps row 3's strip one tab per press, clamped at the ends --- the
    /// same shape the two-tab strip had (Left on Log stays on Log). A
    /// clamped step is a *no-op*: re-selecting the tab the strip is already
    /// on would re-attach a detached shell, so it must not happen.
    pub(super) fn switch_log_tab(&mut self, forward: bool) {
        let tabs = self.available_log_tabs();
        let index = tabs
            .iter()
            .position(|tab| *tab == self.log_tab)
            .unwrap_or(0);
        let next = if forward {
            (index + 1).min(tabs.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        if next != index {
            self.select_log_tab(tabs[next]);
        }
    }

    pub(super) fn select_log_tab(&mut self, tab: LogTab) {
        if tab == LogTab::Terminal {
            // Entering the Terminal tab is the start gesture for its shell
            // (and the re-attach point after a `ctrl+]` detach) --- it never
            // moves focus, so the ctrl chord can flip this strip from
            // another pane without giving up the cursor.
            self.show_terminal_tab();
        } else {
            self.log_tab = tab;
        }
    }

    /// Focus order for `Tab`/`BackTab`. The file columns are stops whenever
    /// row 2 shows the browser --- which is exactly when the backend has no
    /// build panel claiming the row instead (a build backend without a
    /// device filesystem gets the workspace+build pair, `SPEC.md` §11). The
    /// workspace pane is a stop when it exists, the build panel when it is
    /// visible ([`Self::build_pane_visible`]). The Project/Device info row
    /// is never a stop --- it is informational only.
    pub(super) fn focus_order(&self) -> Vec<Focus> {
        let mut order = Vec::new();
        let browser_row = !self.build_pane_visible_precondition();
        if browser_row && self.browser.is_some() {
            order.push(Focus::FilesLocal);
            if self.manager.capabilities().contains(Capability::Filesystem) {
                order.push(Focus::FilesDevice);
            }
        }
        if self.workspace_pane_visible() {
            order.push(Focus::Workspace);
        }
        if self.build_pane_visible() {
            order.push(Focus::Build);
        }
        order.push(Focus::Logs);
        order
    }

    pub(super) fn step_focus(&mut self, forward: bool) {
        let order = self.focus_order();
        let len = order.len();
        if len == 0 {
            return;
        }
        let next = match order.iter().position(|f| *f == self.focus) {
            // The Project pane is off the tour: leaving it forward enters
            // the tour at its first stop, backward at its last --- the
            // detour ends at the tour's ends, whichever way it is left.
            None if forward => 0,
            None => len - 1,
            Some(index) => {
                if forward {
                    (index + 1) % len
                } else {
                    (index + len - 1) % len
                }
            }
        };
        self.focus = order[next];
    }

    /// A focusable pane's shortcut number --- the digit that jumps straight
    /// to it and the number its title carries (`ui::numbered_title`). The
    /// numbers are **fixed per pane position**, not per tour stop:
    /// 1 Environment, 2 Device Info, 3 the left working pane (the local
    /// files column / the workspace pane --- never both), 4 the right one
    /// (the device pane / the build pane), 5 row 3. Fixed on purpose: the
    /// `Tab` tour is dynamic per backend while the numbers must be
    /// memorizable, and the two panes sharing 3 (or 4) are mutually
    /// exclusive by capability. A pane that is not focusable right now
    /// (no backend questions, no browser yet) has no number.
    pub fn pane_number(&self, focus: Focus) -> Option<u8> {
        let browser_row = !self.build_pane_visible_precondition() && self.browser.is_some();
        match focus {
            Focus::Project => (!self.project_rows().is_empty()).then_some(1),
            Focus::DeviceInfo => Some(2),
            Focus::FilesLocal => browser_row.then_some(3),
            Focus::Workspace => self.workspace_pane_visible().then_some(3),
            Focus::FilesDevice => (browser_row
                && self.manager.capabilities().contains(Capability::Filesystem))
            .then_some(4),
            Focus::Build => self.build_pane_visible().then_some(4),
            Focus::Logs => Some(5),
        }
    }

    /// The pane a digit jumps to: the visible occupant of that number's
    /// position (see [`Self::pane_number`] for why two variants can share
    /// a number). `None` when nothing focusable holds the number.
    pub(super) fn pane_for_number(&self, number: u8) -> Option<Focus> {
        [
            Focus::Project,
            Focus::DeviceInfo,
            Focus::FilesLocal,
            Focus::Workspace,
            Focus::FilesDevice,
            Focus::Build,
            Focus::Logs,
        ]
        .into_iter()
        .find(|focus| self.pane_number(*focus) == Some(number))
    }

    /// Which dashboard row a pane lives in: the Environment/Device info
    /// row (1), the working row of file columns and action stacks (2), and
    /// the Log • Monitor • Terminal row (3) --- the geometry the linear
    /// `Tab` tour flattens. Vertical stepping needs the rows back.
    pub(super) fn focus_row(focus: Focus) -> u8 {
        match focus {
            Focus::Project | Focus::DeviceInfo => 1,
            Focus::FilesLocal | Focus::FilesDevice | Focus::Workspace | Focus::Build => 2,
            Focus::Logs => 3,
        }
    }

    /// Which half of its row a pane occupies: the right (`true`: Device
    /// Info, the device column, the build panel) or the left (`false`:
    /// Environment, the local column, the workspace pane). Row 3 spans the
    /// full width and has no half --- `None`.
    pub(super) fn focus_column(focus: Focus) -> Option<bool> {
        match focus {
            Focus::Project | Focus::FilesLocal | Focus::Workspace => Some(false),
            Focus::DeviceInfo | Focus::FilesDevice | Focus::Build => Some(true),
            Focus::Logs => None,
        }
    }

    /// The focusable pane of a dashboard row, preferring the half `right`
    /// names --- visual continuity: `ctrl+↓` from Device Info lands on the
    /// pane directly beneath it (the device column / the build panel), not
    /// on the local column to its left. A half with nothing focusable
    /// falls back to the other (a browser row without a device pane steers
    /// a right-hand descent onto the local column); row 1's right half is
    /// Device Info, always drawn; row 3 has no halves, so either answers
    /// Logs.
    pub(super) fn focusable_in_row(&self, row: u8, right: bool) -> Option<Focus> {
        match row {
            1 => {
                if right || self.project_rows().is_empty() {
                    Some(Focus::DeviceInfo)
                } else {
                    Some(Focus::Project)
                }
            }
            2 => {
                let stops: Vec<Focus> = self
                    .focus_order()
                    .into_iter()
                    .filter(|focus| Self::focus_row(*focus) == 2)
                    .collect();
                stops
                    .iter()
                    .find(|focus| Self::focus_column(**focus) == Some(right))
                    .copied()
                    .or_else(|| stops.first().copied())
            }
            3 => Some(Focus::Logs),
            _ => None,
        }
    }

    /// `ctrl+↓`/`ctrl+↑`: focus the pane of the nearest row below/above
    /// that has one, *staying in the same half of the screen* --- the arrow
    /// follows the dashboard's geometry, and geometry is two-dimensional.
    /// A row with nothing focusable in it is skipped rather than stopping
    /// the walk, and row 3 (no half of its own) is entered from either
    /// side while leaving it upward takes the left half, the reading
    /// order. The vertical half of the chord family `ctrl+←`/`ctrl+→`
    /// completes: the ctrl chords walk the dashboard's structure, the
    /// plain arrows stay the focused pane's own to spend on its content
    /// (directories, rows, buttons).
    pub(super) fn step_focus_vertical(&mut self, down: bool) {
        let step = i8::from(down) * 2 - 1;
        let column = Self::focus_column(self.focus);
        let mut row = Self::focus_row(self.focus) as i8 + step;
        while (1..=3).contains(&row) {
            if let Some(focus) = self.focusable_in_row(row as u8, column.unwrap_or(false)) {
                self.focus = focus;
                return;
            }
            row += step;
        }
    }

    /// The Project pane's way in: jumped to by the shortcuts overlay's `e`
    /// letter (`ctrl+k`), with `Tab` re-entering the tour at its first stop
    /// (the pane is a detour, so the tour is the way back out). Entering
    /// lands the cursor on the first question still open --- the pane exists
    /// to answer what is missing, so that is where the user is put. A pane
    /// with no rows (no backend selected) is not entered at all: there is
    /// nothing to walk, and a letter pressed while already inside is a
    /// no-op.
    pub fn focus_project(&mut self) {
        if self.focus == Focus::Project || self.project_rows().is_empty() {
            return;
        }
        self.focus = Focus::Project;
        self.project_cursor = self.first_open_project_row();
    }

    /// The first pane that still exists after the focused one disappeared:
    /// local files (when the row shows the browser), then workspace, then
    /// build, ending at `Logs` --- the tour's order, so a clamp never jumps
    /// backwards.
    pub(super) fn fallback_pane(&self) -> Focus {
        if !self.build_pane_visible_precondition() && self.browser.is_some() {
            Focus::FilesLocal
        } else if self.workspace_pane_visible() {
            Focus::Workspace
        } else if self.build_pane_visible() {
            Focus::Build
        } else {
            Focus::Logs
        }
    }

    /// Pulls focus back onto a pane that still exists when a backend switch
    /// removed the one it was sitting on: the file columns need both the
    /// browser and the row that shows it, the workspace/build panes need
    /// theirs. Each falls back through [`Self::fallback_pane`].
    pub(super) fn clamp_focus(&mut self) {
        let browser_row = !self.build_pane_visible_precondition();
        let needs_clamp = match self.focus {
            Focus::Project => self.project_rows().is_empty(),
            // Always drawn, whatever the backend or the connection state.
            Focus::DeviceInfo => false,
            Focus::FilesDevice => {
                !browser_row || !self.manager.capabilities().contains(Capability::Filesystem)
            }
            Focus::FilesLocal => !browser_row || self.browser.is_none(),
            Focus::Workspace => !self.workspace_pane_visible(),
            Focus::Build => !self.build_pane_visible(),
            Focus::Logs => false,
        };
        if needs_clamp {
            self.focus = self.fallback_pane();
        }
    }
}
