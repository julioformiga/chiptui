//! Mouse gesture handling: an alternative trigger for actions the keyboard
//! already owns.
//!
//! Reporting is opt-in (`[ui] mouse` in the user config, enabled by
//! `terminal::init`); [`App::handle`](crate::app::App::handle) drops every
//! gesture while it is off, and the event source has already narrowed the
//! stream to left clicks and wheel steps. This module is where a gesture
//! meets the layout: a click focuses the pane it lands on and moves that
//! pane's cursor onto the row it landed on (`ui::layout`'s tree, published
//! frame area and all, is the same geometry the renderer drew), a wheel
//! step scrolls the row-3 pane under it.
//!
//! What a click deliberately does *not* do: confirm. Rows, buttons and
//! tabs all land through the same handlers `Enter` and `←/→` reach, so a
//! click can select, activate and switch exactly what the keyboard can
//! --- and nothing more. A destructive confirm stays a keyboard `Enter`:
//! the shared confirmation overlay is deliberately out of the click's
//! reach until the overlay stage gives it a target it can name.

use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Flex, Layout, Rect};

use crate::app::{
    App, DevicePaneTab, DocsFocus, FileAction, Focus, LogTab, Overlay, ProjectRow, ThemeChoice,
    View,
};
use crate::backend::BackendKind;
use crate::browser::{PaneState, Side, SyncPlan};
use crate::build::BuildAction;
use crate::flash::{FlashAction, FlashPaneAction, FlashPanel, FlashScreen};
use crate::ui::layout::{self, RightKind, Row2};

/// Rows a wheel step scrolls: a notch is a nudge, not a page (the
/// page keys stay the keyboard's).
const WHEEL_STEP: usize = 3;

/// How long after a click the same row's second click counts as a
/// double-click (the terminal reports no double-click event of its own).
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

impl App {
    /// Entry point for [`crate::event::AppEvent::Mouse`].
    pub(super) fn on_mouse(&mut self, event: MouseEvent) {
        // The same interception order `on_key` follows: a modal overlay
        // owns the whole screen, and a gesture under it is not an answer
        // to its question.
        if self.overlay.is_some() {
            self.on_overlay_mouse(event);
            return;
        }
        if self.shortcuts_overlay_active {
            return;
        }
        // The flash view is a dialog layered over the dashboard: its own
        // screens (the online-firmware search among them) take the
        // gesture, the same standing `on_flash_key` has.
        if self.view == View::Flash {
            self.on_flash_mouse(event);
            return;
        }
        if self.view != View::Dashboard {
            return;
        }
        // `ui::draw` publishes this every frame; without it (first frame,
        // terminal too small) there is no geometry to hit.
        let Some(frame) = self.frame_area else { return };
        // The frame's own split: one header row, the body, one footer row
        // (`ui::draw`'s `Layout` --- mirrored here, not shared, because it
        // is three constant rows).
        let body = Rect {
            y: frame.y + 1,
            height: frame.height.saturating_sub(2),
            ..frame
        };
        let areas = layout::dashboard(self, body);
        let point = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(point, &areas),
            MouseEventKind::ScrollUp => self.wheel(-1, point, &areas),
            MouseEventKind::ScrollDown => self.wheel(1, point, &areas),
            _ => {}
        }
    }

    /// A left click: focus the pane it lands on, then move that pane's
    /// cursor onto the row under the pointer.
    fn click(&mut self, point: (u16, u16), areas: &layout::DashboardAreas) {
        // A monitor or terminal session owns the keyboard; while one is
        // live the click has nothing to say (its panes' rows are not the
        // conversation). The wheel below still scrolls, the way
        // shift+pgup does beside a live shell.
        if self.is_monitor_active() || self.is_terminal_active() {
            return;
        }

        // The header's project name is a shortcut: clicking it is
        // shift+P (the project list), the same synthesized-key rule the
        // overlays follow --- so the switch-confirm question a running
        // command deserves opens exactly as it does for the keyboard.
        // Row 1 starts one row below the header (`frame.y + 1`), so the
        // header itself is `project.y - 1`.
        if point.1 == areas.project.y.saturating_sub(1) {
            let frame = self.frame_area.unwrap_or_default();
            let palette = self.theme_palette();
            if let Some(name) = crate::ui::header_project_name_rect(frame, self, palette)
                && contains(name, point)
            {
                self.on_key(ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Char('P'),
                    KeyModifiers::SHIFT,
                ));
                return;
            }
        }

        // Row 1.
        if contains(areas.project, point) {
            self.click_project(point, areas.project);
            return;
        }
        if contains(areas.device, point) {
            // The Device Info pane is informational and never focused ---
            // a click on it is not a focus claim. Its one clickable fact
            // is the MAC: the identity a user copies into a sticker, a
            // router binding, a bug report.
            let mac = self
                .flash
                .as_ref()
                .and_then(|flash| flash.details.mac.clone());
            if let Some(mac) = mac
                && let Some(row) =
                    crate::ui::device_mac_row(self, areas.device.width.saturating_sub(2) as usize)
                && inner_row(point, areas.device, 0, 0) == Some(row)
            {
                self.copy_to_clipboard("MAC", mac);
            }
            return;
        }

        // Row 2.
        match areas.row2 {
            Row2::WorkspaceBuild { workspace, build } => {
                if contains(workspace, point) {
                    self.click_workspace(point, workspace);
                } else if contains(build, point) {
                    self.click_build_stack(point, build);
                }
            }
            Row2::Browser(row) => {
                if contains(row.local, point) {
                    self.click_local(point, row.local);
                } else if contains(row.right, point) {
                    match row.right_kind {
                        RightKind::Device => self.click_device_pane(point, row.right),
                        // The build pane beside a browser row has its own
                        // stack to activate; `NoDevice` has no rows.
                        RightKind::Build => self.click_build_stack(point, row.right),
                        RightKind::NoDevice => {}
                    }
                }
            }
            Row2::Placeholder(_) => {}
        }

        // Row 3: one pane, whose top border carries the tab strip. A click
        // on a tab switches to it; on the body it focuses the pane.
        if contains(areas.row3, point) {
            if let Some(tab) = strip_tab(point, areas.row3, &self.log_strip_tabs()) {
                self.select_log_tab(tab);
            } else if point.1 > areas.row3.y {
                self.focus = Focus::Logs;
            }
        }
    }

    /// The Environment pane's checklist: focus plus the clicked row's
    /// cursor. The pane is never scrolled (`panels::INFO_ROWS` fixed), so
    /// the row under the pointer is the row that gets the cursor. A click
    /// is also the row's `Enter` --- every row's predefined action is a
    /// dialog (a picker), so selecting and asking are the same gesture.
    fn click_project(&mut self, point: (u16, u16), rect: Rect) {
        let rows = self.project_rows();
        let len = rows.len();
        if len == 0 {
            // Nothing to walk: the pane is plain detection info and stays
            // out of focus (`focus_project`'s own rule).
            return;
        }
        self.focus = Focus::Project;
        if let Some(index) = inner_row(point, rect, 0, 0) {
            self.project_cursor = index.min(len - 1);
            // The merged `Board · Shield` row carries two dialogs: the
            // click sets the half it landed on before asking, so the
            // picker that opens is the one under the pointer, not the one
            // `←`/`→` last selected.
            if rows[self.project_cursor] == ProjectRow::BoardShield {
                let column = point.0.saturating_sub(rect.x + 1);
                self.board_segment = crate::ui::board_shield_click_is_board(
                    self,
                    rect.width.saturating_sub(2),
                    self.theme_palette(),
                    column,
                );
            }
            self.on_key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ));
        }
    }

    /// The browser's local pane.
    fn click_local(&mut self, point: (u16, u16), rect: Rect) {
        self.focus = Focus::FilesLocal;
        let Some(browser) = self.browser.as_mut() else {
            return;
        };
        browser.focus = Side::Local;
        if let Some(index) = list_row(
            point,
            rect,
            browser.local_cursor,
            browser.visible_local().len(),
            0,
            0,
        ) {
            browser.local_cursor = index;
            self.maybe_double_click(Focus::FilesLocal, index);
        }
    }

    /// The browser's device pane, while the files tab is showing a ready
    /// listing (any other state draws no rows to click). The actions tab
    /// is a button stack and lands through [`Self::click_flash_stack`].
    fn click_device(&mut self, point: (u16, u16), rect: Rect) {
        self.focus = Focus::FilesDevice;
        let Some(browser) = self.browser.as_mut() else {
            return;
        };
        browser.focus = Side::Device;
        if !matches!(browser.device_state, PaneState::Ready) {
            return;
        }
        // The pane's last inner row is the usage footer (`draw_device_
        // footer`), not a list row.
        if let Some(index) = list_row(
            point,
            rect,
            browser.device_cursor,
            browser.visible_device().len(),
            0,
            1,
        ) {
            browser.device_cursor = index;
            self.maybe_double_click(Focus::FilesDevice, index);
        }
    }

    /// The workspace pane's project-files list.
    fn click_workspace(&mut self, point: (u16, u16), rect: Rect) {
        let Some(panel) = self.workspace.as_ref() else {
            return;
        };
        if panel.files_error.is_some() {
            // An error paragraph owns the pane; there are no rows to pick.
            return;
        }
        let hit = list_row(
            point,
            rect,
            panel.files_cursor,
            panel.files_row_count(),
            0,
            0,
        );
        self.focus = Focus::Workspace;
        if let Some(index) = hit {
            self.workspace.as_mut().unwrap().files_cursor = index;
            self.maybe_double_click(Focus::Workspace, index);
        }
    }

    /// A second click on the same row soon enough is the row's `Enter`:
    /// in the browser that opens the entry's action menu, in the Zephyr
    /// Files pane it descends into the directory or opens the file in
    /// `$EDITOR` --- whatever `Enter` means in the pane that was clicked,
    /// never a meaning of its own.
    fn maybe_double_click(&mut self, pane: Focus, index: usize) {
        let now = std::time::Instant::now();
        let repeat = matches!(self.last_click, Some((p, i, at))
            if p == pane && i == index && now.duration_since(at) < DOUBLE_CLICK);
        self.last_click = if repeat {
            None
        } else {
            Some((pane, index, now))
        };
        if repeat {
            self.on_key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ));
        }
    }

    /// The device pane as a whole: its tab strip first (the files/actions
    /// switch, the same targets `ctrl+←/→` flip), then the tab's own
    /// content --- the files listing or the actions stack.
    fn click_device_pane(&mut self, point: (u16, u16), rect: Rect) {
        if strip_tab(point, rect, &self.device_strip_tabs()).is_some() {
            // A click on the strip flips the tab the same way the chord
            // does: the target tab, created if the click is what brings
            // the panel into being.
            if self.device_actions_tab_active() {
                self.device_pane_tab = DevicePaneTab::Files;
            } else {
                self.show_device_actions_tab();
            }
            return;
        }
        if self.device_actions_tab_active() {
            self.click_flash_stack(point, rect);
        } else {
            self.click_device(point, rect);
        }
    }

    /// A wheel step over row 3 scrolls its active tab --- the same
    /// handlers `↑`/`↓` reach. It never moves focus: scrolling past a pane
    /// is not pointing at it.
    fn wheel(&mut self, direction: isize, point: (u16, u16), areas: &layout::DashboardAreas) {
        if !contains(areas.row3, point) {
            return;
        }
        let steps = WHEEL_STEP;
        match self.log_tab {
            LogTab::Log => {
                if direction < 0 {
                    self.logs.scroll_up(steps, self.log_viewport);
                } else {
                    self.logs.scroll_down(steps);
                }
            }
            LogTab::Monitor | LogTab::Terminal => {
                if direction < 0 {
                    self.monitor_scroll_up(steps);
                } else {
                    self.monitor_scroll_down(steps);
                }
            }
        }
    }

    /// The build pane's stacked buttons: a click on a button's row selects
    /// it *and* activates it --- the exact `Enter` path
    /// (`run_build_action`, dimmed-rows-are-no-ops included). A click on
    /// the rules, dividers or the reserved footer's state line does
    /// nothing; the footer's `Stop` box is the one footer row that acts.
    fn click_build_stack(&mut self, point: (u16, u16), rect: Rect) {
        self.focus = Focus::Build;
        let Some(panel) = self.build.as_ref() else {
            return;
        };
        let caps = self.manager.capabilities();
        let actions = panel.actions(&caps);
        let stop = usize::from(panel.is_busy() && actions.last() == Some(&BuildAction::Stop));
        let mains = actions.len() - stop;
        // The stack starts at the pane's inner top row; button i's label
        // sits `1 + 2i` rows below it (a rule between each pair).
        let stack_y = rect.y + 1;
        let Some(row) = point
            .1
            .checked_sub(stack_y)
            .filter(|row| *row < 2 * mains as u16 + 1)
        else {
            return;
        };
        // Odd offsets are button labels (the top rule is 0, a divider
        // follows every button); `row / 2` is the button's index.
        if row % 2 == 1 {
            let index = (row / 2) as usize;
            let action = actions[index];
            self.build.as_mut().unwrap().cursor = index;
            self.run_build_action(action);
        }
    }

    /// The flash actions tab's stacked buttons, the same shape as the
    /// build pane's (`run_flash_pane_action`, whose gates are the run's:
    /// busy rows warn rather than start, destructive ones ask).
    fn click_flash_stack(&mut self, point: (u16, u16), rect: Rect) {
        self.focus = Focus::FilesDevice;
        let Some(flash) = self.flash.as_ref() else {
            return;
        };
        let actions = flash.pane_actions();
        let stop = usize::from(flash.is_busy() && actions.last() == Some(&FlashPaneAction::Stop));
        let mains = actions.len() - stop;
        let stack_y = rect.y + 1;
        let Some(row) = point
            .1
            .checked_sub(stack_y)
            .filter(|row| *row < 2 * mains as u16 + 1)
        else {
            return;
        };
        // Odd offsets are button labels, the same stack grammar the build
        // pane follows.
        if row % 2 == 1 {
            let index = (row / 2) as usize;
            let action = actions[index];
            self.flash.as_mut().unwrap().pane_cursor = index;
            self.run_flash_pane_action(action);
        }
    }

    /// The titles a pane's tab strip draws, in draw order --- the two
    /// strips' label sequences, kept beside the hit-tester that maps a
    /// click onto one of them. Built with the session's own icon set, so
    /// the widths match the strip the frame drew (`none` drops the glyph
    /// and its gap, changing the ranges).
    fn log_strip_tabs(&self) -> Vec<(LogTab, String)> {
        let icons = self.icon_set();
        self.available_log_tabs()
            .into_iter()
            .map(|tab| {
                let title = match tab {
                    LogTab::Log => "Log",
                    LogTab::Monitor => "Monitor",
                    LogTab::Terminal => "Terminal",
                };
                let glyph = match tab {
                    LogTab::Log => icons.list(),
                    LogTab::Monitor => icons.screen(),
                    LogTab::Terminal => icons.prompt(),
                };
                (tab, crate::ui::pane_title(glyph, title))
            })
            .collect()
    }

    fn device_strip_tabs(&self) -> Vec<(DevicePaneTab, String)> {
        let icons = self.icon_set();
        [
            (DevicePaneTab::Actions, icons.bolt(), "Actions"),
            (DevicePaneTab::Files, icons.folder(), "Device Files"),
        ]
        .into_iter()
        .map(|(tab, glyph, title)| (tab, crate::ui::pane_title(glyph, title)))
        .collect()
    }

    /// The open overlay's own click handling, reached instead of the
    /// dashboard's while a modal owns the screen (the same standing
    /// `on_overlay_key` has). A click outside the dialog is *ignored*
    /// everywhere: dismissing a destructive confirm by mis-clicking beside
    /// it would be cheaper than `Esc` for the wrong question.
    ///
    /// The grammar, one rule per shape: a confirm's `No`/`Yes` buttons
    /// answer the question directly (a click on a drawn button is as
    /// explicit as `y`/`n` --- synthesized as exactly those keys, so every
    /// per-variant accept/decline path is the keyboard's own); a picker's
    /// rows select (`Enter` stays the activation); a stacked-button menu
    /// (Zephyr Actions, the installer's footer button) selects and presses
    /// through `Enter`; the SDK checklist's rows toggle, the way a checkbox
    /// click means `Space`. Input dialogs, the viewer and the help window
    /// have no click surface --- their gestures fall through, swallowed.
    fn on_overlay_mouse(&mut self, event: MouseEvent) {
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let Some(frame) = self.frame_area else { return };
        let point = (event.column, event.row);
        let Some(overlay) = self.overlay.clone() else {
            return;
        };
        match overlay {
            // ---- confirms: the drawn No/Yes buttons answer -------------
            Overlay::Confirm { .. }
            | Overlay::ConfirmBuild { .. }
            | Overlay::ConfirmRestartDevice { .. }
            | Overlay::ConfirmSwitchProject { .. }
            | Overlay::ConfirmEraseForMicroPython { .. }
            | Overlay::ConfirmInterruptDevice { .. }
            | Overlay::ConfirmDelete { .. }
            | Overlay::ConfirmDownloadOverwrite { .. }
            | Overlay::ConfirmUpload { .. }
            | Overlay::SyncPreview { .. }
            | Overlay::ConfirmInstallHere { .. } => {
                let Some((no, yes)) = confirm_buttons(frame, self.confirm_size(&overlay, frame))
                else {
                    return;
                };
                if contains(no, point) {
                    self.overlay_key(KeyCode::Char('n'));
                } else if contains(yes, point) {
                    self.overlay_key(KeyCode::Char('y'));
                }
            }

            // ---- pickers: a click selects the row ----------------------
            Overlay::DevicePicker { selected } => {
                let len = self.devices.devices().len();
                if let Some(index) = list_row(
                    point,
                    crate::ui::centered(frame, 64, len as u16 + 2),
                    selected,
                    len,
                    0,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::ThemePicker { selected } => {
                let len = ThemeChoice::all().len();
                if let Some(index) = list_row(
                    point,
                    crate::ui::centered(frame, 44, len as u16 + 2),
                    selected,
                    len,
                    0,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::FirmwarePicker { selected } => {
                let Some(len) = self.flash.as_ref().map(|flash| flash.firmware.len()) else {
                    return;
                };
                if len == 0 {
                    return;
                }
                if let Some(index) = list_row(
                    point,
                    crate::ui::centered(frame, 64, len as u16 + 2),
                    selected,
                    len,
                    0,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::ProjectSetup { selected } => {
                let len = BackendKind::ALL.len();
                if let Some(index) = list_row(
                    point,
                    crate::ui::centered(frame, 60, len as u16 + 4),
                    selected,
                    len,
                    2,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::RestoreDeviceScript { selected } => {
                // Three constant choices under a two-row message.
                if let Some(index) =
                    list_row(point, crate::ui::centered(frame, 64, 7), selected, 3, 2, 0)
                {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::FileActions {
                side,
                name,
                is_dir,
                status,
                selected,
            } => {
                let is_text = crate::files::is_text_like(&name);
                let actions = FileAction::for_entry(
                    side,
                    is_dir,
                    is_text,
                    status,
                    self.manager.capabilities(),
                );
                let len = actions.len();
                if let Some(index) = list_row(
                    point,
                    crate::ui::centered(frame, 44, len as u16 + 2),
                    selected,
                    len,
                    0,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::DirPicker { path, selected, .. } => {
                let len = crate::workspace::dir_rows(&path).0.len();
                if let Some(index) = list_row(
                    point,
                    crate::ui::centered(frame, 72, 18),
                    selected,
                    len,
                    1,
                    2,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::ProjectPicker { mpy, selected, .. } => {
                let dir = if mpy {
                    self.mpy_projects.clone()
                } else {
                    self.workspace
                        .as_ref()
                        .and_then(|panel| panel.projects.clone())
                };
                let len = dir
                    .map(|dir| picker_project_rows(&dir, mpy))
                    .unwrap_or_default();
                if let Some(index) = list_row(
                    point,
                    crate::ui::centered(frame, 72, 18),
                    selected,
                    len,
                    1,
                    2,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::BuildDirPicker { input, selected } => {
                let Some(len) = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_build_dirs(&input).len())
                else {
                    return;
                };
                if let Some(index) = list_row(
                    point,
                    crate::ui::centered(frame, 60, 16),
                    selected,
                    len,
                    3,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::BoardPicker {
                input, selected, ..
            } => {
                // A click on the details pane hands it the keyboard (the
                // mouse's way of `Tab`); a click on the list side hands it
                // back and selects the row under the pointer.
                let areas = layout::docs_picker(frame);
                if contains(areas.details, point) {
                    self.set_docs_picker_focus(DocsFocus::Details);
                    return;
                }
                self.set_docs_picker_focus(DocsFocus::List);
                let Some(len) = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_boards(&input).len())
                else {
                    return;
                };
                if let Some(index) = docs_list_row(frame, point, selected, len) {
                    self.set_docs_picker_selection(index);
                }
            }
            Overlay::ShieldPicker {
                input, selected, ..
            } => {
                // Same grammar as the board picker, over a list whose row 0
                // is the `(none)` row --- it is included in the count.
                let areas = layout::docs_picker(frame);
                if contains(areas.details, point) {
                    self.set_docs_picker_focus(DocsFocus::Details);
                    return;
                }
                self.set_docs_picker_focus(DocsFocus::List);
                let Some(len) = self
                    .build
                    .as_ref()
                    .map(|panel| 1 + panel.filtered_shields(&input).len())
                else {
                    return;
                };
                if let Some(index) = docs_list_row(frame, point, selected, len) {
                    self.set_docs_picker_selection(index);
                }
            }

            // ---- checklist: a row click toggles, the checkbox meaning --
            Overlay::SdkToolchains { selected } => {
                let len = crate::install::steps::TOOLCHAINS.len();
                let popup = crate::ui::centered(frame, 56, frame.height.saturating_sub(4));
                if let Some(index) = list_row(point, popup, selected, len, 0, 1) {
                    self.set_overlay_selected(index);
                    self.overlay_key(KeyCode::Char(' '));
                }
            }

            // ---- stacked buttons: select and press ---------------------
            Overlay::ZephyrActions { .. } => {
                // Three two-row buttons (label + detail) with dividers and
                // the outer rules: popup height is the stack's 10 + borders.
                let popup = crate::ui::centered(frame, 64, 12);
                let Some(row) = point.1.checked_sub(popup.y + 1).filter(|row| *row < 10) else {
                    return;
                };
                // Bands start after the top rule; each button owns two rows
                // (label + detail) with a divider between pairs, so the
                // *skip* rows are exactly the multiples of three (the top
                // rule and the two dividers) and both the label and the
                // command's description under it press the button.
                let index = (row / 3) as usize;
                if row % 3 != 0 && index < 3 {
                    self.set_overlay_selected(index);
                    self.overlay_key(KeyCode::Enter);
                }
            }
            Overlay::ZephyrInstall => {
                // The installer's footer button, pinned to the modal's
                // bottom rows on the right --- the same box `Enter` presses.
                let popup = crate::ui::install_area(frame);
                let inner = Rect {
                    x: popup.x + 1,
                    y: popup.y + 1,
                    width: popup.width.saturating_sub(2),
                    height: popup.height.saturating_sub(2),
                };
                let footer_top = inner.y + inner.height.saturating_sub(3);
                let stop = crate::ui::STOP_BOX_WIDTH.min(inner.width);
                let button = Rect {
                    x: inner.x + inner.width - stop,
                    width: stop,
                    y: footer_top,
                    height: inner.height.saturating_sub(footer_top - inner.y),
                };
                if contains(button, point) {
                    self.overlay_key(KeyCode::Enter);
                }
            }

            // Input dialogs, the viewer and the help window: no click
            // surface (their one meaningful gesture is typing).
            _ => {}
        }
    }

    /// The flash dialog's own clicks (`View::Flash`), the same standing
    /// `on_flash_key` has over the dashboard. The online screens are the
    /// reason this exists: selecting a board and downloading its firmware
    /// are list gestures, and the search results are rows a pointer can
    /// land on. Rows select *and* activate --- `Enter` synthesized into
    /// the screen's own handler, so the firmware picker's overwrite
    /// question and every other gate apply unchanged. The free-text URL
    /// screen is a typed answer with no click surface; the options screen
    /// only moves its field focus (its `Enter` opens a text edit).
    fn on_flash_mouse(&mut self, event: MouseEvent) {
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let Some(frame) = self.frame_area else { return };
        let Some(flash) = self.flash.as_ref() else {
            return;
        };
        let point = (event.column, event.row);
        let (width, height) = crate::ui::flash_dialog_size(flash);
        // The dialog centers over the body (`draw_flash_dialog`'s rect).
        let body = Rect {
            y: frame.y + 1,
            height: frame.height.saturating_sub(2),
            ..frame
        };
        let popup = crate::ui::centered(body, width, height);
        let inner = Rect {
            x: popup.x + 1,
            y: popup.y + 1,
            width: popup.width.saturating_sub(2),
            height: popup.height.saturating_sub(2),
        };
        if !contains(inner, point) {
            return;
        }
        match flash.screen {
            FlashScreen::OnlineBoards => {
                let len = flash.online_boards.len();
                // The table's own header leads its rows.
                let list = inner_shrink(inner, 2, 2);
                if let Some(index) = bare_list_row(point, list, flash.online_cursor, len, 1) {
                    self.flash.as_mut().unwrap().online_cursor = index;
                    self.flash_key(KeyCode::Enter);
                }
            }
            FlashScreen::OnlineFirmware => {
                let len = flash.online_firmware.len();
                let list = inner_shrink(inner, 2, 2);
                if let Some(index) = bare_list_row(point, list, flash.online_cursor, len, 0) {
                    self.flash.as_mut().unwrap().online_cursor = index;
                    self.flash_key(KeyCode::Enter);
                }
            }
            FlashScreen::Menu => {
                let len = FlashAction::ALL.len();
                // The menu is a borderless list inside the dialog's inner
                // rect: the same fresh-`ListState` minimal-scroll math with
                // no borders to discount.
                if let Some(index) = bare_list_row(point, inner, flash.cursor, len, 0) {
                    self.flash.as_mut().unwrap().cursor = index;
                    self.flash_key(KeyCode::Enter);
                }
            }
            FlashScreen::Options => {
                // Label, blank, then one row per field: a click focuses
                // the field it lands on (arrows/Enter stay the editor's).
                let fields = FlashPanel::options_fields(flash.selected_action());
                if let Some(row) = point.1.checked_sub(inner.y + 2) {
                    let index = row as usize;
                    if let Some(field) = fields.get(index) {
                        self.flash.as_mut().unwrap().options_focus = *field;
                    }
                }
            }
            FlashScreen::CustomUrl => {}
        }
    }

    /// Synthesizes a keypress into the flash dialog's handler, the same
    /// trick `overlay_key` uses for modals.
    fn flash_key(&mut self, code: KeyCode) {
        let key = ratatui::crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        self.on_flash_key(key);
    }

    /// Writes `index` into whichever `selected` field the open overlay
    /// carries --- every picker variant names it the same way.
    fn set_overlay_selected(&mut self, index: usize) {
        if let Some(
            Overlay::DevicePicker { selected, .. }
            | Overlay::ThemePicker { selected, .. }
            | Overlay::FirmwarePicker { selected, .. }
            | Overlay::ProjectSetup { selected, .. }
            | Overlay::RestoreDeviceScript { selected, .. }
            | Overlay::FileActions { selected, .. }
            | Overlay::DirPicker { selected, .. }
            | Overlay::ProjectPicker { selected, .. }
            | Overlay::BuildDirPicker { selected, .. }
            | Overlay::BoardPicker { selected, .. }
            | Overlay::ShieldPicker { selected, .. }
            | Overlay::SdkToolchains { selected, .. }
            | Overlay::ZephyrActions { selected, .. },
        ) = &mut self.overlay
        {
            *selected = index;
        }
    }

    /// A docs picker's focus follows the click: a gesture on the details
    /// pane hands it the keyboard, one on the list side takes it back ---
    /// the same swap `Tab` makes.
    fn set_docs_picker_focus(&mut self, focus: DocsFocus) {
        if let Some(
            Overlay::BoardPicker { focus: picker, .. }
            | Overlay::ShieldPicker { focus: picker, .. },
        ) = &mut self.overlay
        {
            *picker = focus;
        }
    }

    /// A docs picker's selection change: the row under the cursor and the
    /// details pane's scroll reset together, the same pair the keyboard's
    /// cursor moves reset.
    fn set_docs_picker_selection(&mut self, index: usize) {
        if let Some(
            Overlay::BoardPicker {
                selected, scroll, ..
            }
            | Overlay::ShieldPicker {
                selected, scroll, ..
            },
        ) = &mut self.overlay
        {
            *selected = index;
            *scroll = 0;
        }
    }

    /// Synthesizes a keypress into the overlay's own handler --- how a
    /// click lands on the exact `Enter`/`y`/`n`/`Space` path the keyboard
    /// takes, per variant behavior included.
    fn overlay_key(&mut self, code: KeyCode) {
        let key = ratatui::crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        self.on_overlay_key(key);
    }

    /// The confirm-family dialog's `(width, height)`, each the size its
    /// own draw call uses (`draw_confirm_dialog`'s callers in
    /// `ui::overlay`) --- the shared button block geometry needs only the
    /// popup's rect, and the render-pinned tests keep this table honest.
    fn confirm_size(&self, overlay: &Overlay, frame: Rect) -> (u16, u16) {
        match overlay {
            Overlay::Confirm { .. } if self.install_confirm_pending => (72, 10),
            Overlay::Confirm { .. }
                if self
                    .flash
                    .as_ref()
                    .is_some_and(|flash| flash.pending().is_some()) =>
            {
                (72, 9)
            }
            Overlay::Confirm { .. } => (70, 7),
            Overlay::ConfirmBuild { .. } => (72, 9),
            Overlay::ConfirmRestartDevice { .. } => (54, 8),
            Overlay::ConfirmSwitchProject { .. } => (62, 9),
            Overlay::ConfirmEraseForMicroPython { .. } => (65, 9),
            Overlay::ConfirmInterruptDevice { .. } => (64, 10),
            Overlay::ConfirmDelete { .. } => (54, 9),
            Overlay::ConfirmDownloadOverwrite { .. } => (70, 8),
            Overlay::ConfirmUpload { .. } => (65, 8),
            Overlay::ConfirmInstallHere { .. } => (72, 9),
            Overlay::SyncPreview { plan, .. } if plan.is_empty() => (58, 7),
            Overlay::SyncPreview { plan, .. } => {
                let lines = sync_preview_lines(plan);
                (
                    60,
                    (lines + 5).min(frame.height.saturating_sub(2) as usize) as u16,
                )
            }
            _ => (0, 0),
        }
    }
}

/// Maps a click onto a row of a *borderless* list rect (a list drawn
/// inside a dialog's inner area) --- [`list_row`] minus the two border
/// rows, with `header` leading rows the clickable entries do not own.
fn bare_list_row(
    point: (u16, u16),
    list: Rect,
    selected: usize,
    len: usize,
    header: u16,
) -> Option<usize> {
    if len == 0 || list.height == 0 || !contains(list, point) {
        return None;
    }
    let header = header as usize;
    let height = list.height as usize;
    if height <= header {
        return None;
    }
    let offset = selected.saturating_sub(height - 1 - header);
    let index = offset + (point.1 - list.y) as usize - header;
    (index < len).then_some(index)
}

/// `rect` shrunk by `top`/`bottom` rows --- the head/foot strips an online
/// screen draws around its list.
fn inner_shrink(rect: Rect, top: u16, bottom: u16) -> Rect {
    Rect {
        y: rect.y + top,
        height: rect.height.saturating_sub(top + bottom),
        ..rect
    }
}

/// Whether `rect` contains the point --- Ratatui `Rect` has no `contains`
/// for positions.
fn contains(rect: Rect, point: (u16, u16)) -> bool {
    rect.x <= point.0
        && point.0 < rect.x + rect.width
        && rect.y <= point.1
        && point.1 < rect.y + rect.height
}

/// The row index inside a bordered pane's inner area, `None` on the
/// borders or past the bottom. `skip` counts leading inner rows the
/// clickable list does not own (a message above a picker's list);
/// `reserved` counts trailing ones (the device pane's footer).
fn inner_row(point: (u16, u16), rect: Rect, skip: u16, reserved: u16) -> Option<usize> {
    let inner_y = rect.y + 1 + skip;
    let height = rect.height.saturating_sub(2 + skip + reserved);
    (point.1 >= inner_y && point.1 < inner_y + height).then(|| (point.1 - inner_y) as usize)
}

/// Maps a click inside a bordered pane to an index in the list that pane
/// draws --- or `None` when the click is on the borders, below the list,
/// or on a row no entry occupies. The second half is the load-bearing
/// trick: the panes draw with a *fresh* `ListState` every frame
/// (`render_list`), so the scroll offset ratatui settles on is the
/// minimal one that keeps `selected` visible --- a pure function of the
/// selection and the height. Reproducing it here (rather than caching
/// offsets through the renderer) keeps the mapping in one testable place;
/// the render test below pins it against what the terminal actually drew.
fn list_row(
    point: (u16, u16),
    rect: Rect,
    selected: usize,
    len: usize,
    skip: u16,
    reserved: u16,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let row = inner_row(point, rect, skip, reserved)?;
    let height = rect.height.saturating_sub(2 + skip + reserved) as usize;
    if height == 0 {
        return None;
    }
    // The minimal-scroll offset: the selection pinned at the bottom edge
    // once it passes it, the top of the list otherwise.
    let offset = selected.saturating_sub(height - 1);
    let index = offset + row;
    (index < len).then_some(index)
}

/// The tab a click on a pane's top-border strip selected, `None` when the
/// click missed every label. Both strips draw with the Ratatui `Tabs`
/// widget's grammar (`ui::panels::draw_log_tabs`,
/// `ui::files::draw_device_tabs`): each tab is `pad + title + pad` wide,
/// a one-column divider between neighbours. The caller supplies the
/// `(tab, title)` pairs in draw order (`log_strip_tabs`/
/// `device_strip_tabs` build them from the same enums the renderers do),
/// so the ranges walked here are exactly the ones drawn --- a `Tabs`
/// grammar change means updating this walk and the two builders beside it.
fn strip_tab<T: Copy>(point: (u16, u16), pane: Rect, tabs: &[(T, String)]) -> Option<T> {
    // The strip is the pane's top border row between the corners.
    if point.1 != pane.y || pane.width < 2 {
        return None;
    }
    let strip_start = pane.x + 1;
    let strip_end = pane.x + pane.width - 1;
    if point.0 < strip_start || point.0 >= strip_end {
        return None;
    }
    let mut x = strip_start;
    for (position, (tab, title)) in tabs.iter().enumerate() {
        let width = 2 + title.chars().count() as u16; // pad + title + pad
        let start = x;
        x += width;
        if position + 1 < tabs.len() {
            x += 1; // the DOT divider
        }
        if point.0 >= start && point.0 < x {
            return Some(*tab);
        }
        if x > point.0 {
            return None;
        }
    }
    None
}

/// The `No`/`Yes` button rects of a `draw_confirm_dialog`-shaped modal ---
/// re-derived with the exact `Layout` calls the renderer uses (the same
/// `centered`, the same vertical message/buttons split, the same centered
/// `10/4/10` horizontal split), so a click lands on the box that was drawn
/// without a second opinion about rounding.
fn confirm_buttons(area: Rect, size: (u16, u16)) -> Option<(Rect, Rect)> {
    let (width, height) = size;
    if width == 0 || height == 0 {
        return None;
    }
    let popup = crate::ui::centered(area, width.min(area.width), height);
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let [_, buttons] = Layout::vertical([
        Constraint::Length(height.saturating_sub(5)),
        Constraint::Length(3),
    ])
    .areas(inner);
    let [no, _gap, yes] = Layout::horizontal([
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Length(10),
    ])
    .flex(Flex::Center)
    .areas(buttons);
    Some((no, yes))
}

/// A row click inside a docs picker's west list (the board/shield pickers'
/// two-column modal): the rows sit inside the pane's border, on the shared
/// geometry the frame drew ([`layout::docs_picker`]). Same fresh-`ListState`
/// minimal-scroll math as [`list_row`], over the list's explicit rect.
fn docs_list_row(area: Rect, point: (u16, u16), selected: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let pane = layout::docs_picker(area).list;
    let inner = Rect {
        x: pane.x + 1,
        y: pane.y + 1,
        width: pane.width.saturating_sub(2),
        height: pane.height.saturating_sub(2),
    };
    if !contains(inner, point) || inner.height == 0 {
        return None;
    }
    let height = inner.height as usize;
    let offset = selected.saturating_sub(height - 1);
    let index = offset + (point.1 - inner.y) as usize;
    (index < len).then_some(index)
}

/// The line count `draw_sync_preview` builds for `plan` --- the number its
/// own height formula uses, replicated here so the click knows where the
/// dialog's buttons sit. Kept beside the render-pinned tests.
fn sync_preview_lines(plan: &SyncPlan) -> usize {
    fn section(len: usize, trailing_blank: bool) -> usize {
        // Header + up to eight rows + an "... and N more" when clipped.
        let mut rows = 1 + len.min(8) + usize::from(len > 8);
        if trailing_blank {
            rows += 1;
        }
        rows
    }
    let mut lines = 0;
    if !plan.uploads.is_empty() {
        lines += section(plan.uploads.len(), true);
    }
    if !plan.mkdirs.is_empty() {
        lines += section(plan.mkdirs.len(), true);
    }
    if !plan.deletes.is_empty() {
        lines += section(plan.deletes.len(), false);
    }
    lines
}

/// The project picker's row count: the configured folder's immediate
/// subdirectories, whichever backend flavour is asking.
fn picker_project_rows(dir: &std::path::Path, mpy: bool) -> usize {
    if mpy {
        crate::backend::micropython::projects::project_rows(dir)
            .0
            .len()
    } else {
        crate::backend::zephyr::projects::project_rows(dir).0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppEvent;
    use crate::backend::BackendKind;
    use crate::browser::Browser;
    use ratatui::crossterm::event::KeyModifiers;

    /// Renders `app` into a `TestBackend` terminal and returns the drawn
    /// lines --- the same smoke pattern `tests/ui_render.rs` uses, so a
    /// click can be aimed at a row whose *drawn* content we know.
    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        terminal
            .backend()
            .to_string()
            .lines()
            .map(String::from)
            .collect()
    }

    fn gesture(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.handle(AppEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn click(app: &mut App, column: u16, row: u16) {
        gesture(app, MouseEventKind::Down(MouseButton::Left), column, row);
    }

    fn wheel(app: &mut App, direction: isize, column: u16, row: u16) {
        let kind = if direction < 0 {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        };
        gesture(app, kind, column, row);
    }

    /// A hermetic `App` with the backend forced and mouse gestures live;
    /// `frame_area` comes from a real render so the click aims at what a
    /// frame actually drew.
    fn app_with_backend(kind: BackendKind, root: &std::path::Path) -> App {
        let mut app = App::new(root);
        let home = std::env::temp_dir().join(format!(
            "chiptui-mouse-home-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        app.set_home_dir(home);
        app.manager.set_override(Some(kind));
        app.bootstrap();
        // The dashboard's real startup path: the scan is what creates the
        // Zephyr workspace+build panes (and leaves MicroPython's browser
        // alone), and an empty fixture /dev keeps it offline.
        let empty_dev = std::env::temp_dir().join(format!(
            "chiptui-mouse-dev-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&empty_dev).unwrap();
        app.set_serial_dir(&empty_dev);
        app.maybe_scan_devices();
        app.set_mouse_enabled(true);
        app
    }

    /// A project root with `count` files named to sort in order, plus the
    /// `CMakeLists.txt` that makes the directory a buildable Zephyr app.
    fn project_dir(tag: &str, count: usize) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "chiptui-mouse-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.28)\n",
        )
        .unwrap();
        for i in 0..count {
            std::fs::write(root.join(format!("file{i:02}.py")), "x = 1\n").unwrap();
        }
        root
    }

    #[test]
    fn a_click_on_the_local_pane_picks_the_drawn_row() {
        let root = project_dir("local", 30);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.browser = Some(Browser::new(&root));
        let lines = render(&mut app, 100, 40);
        // `render` published frame_area; a click on the local pane's first
        // inner row selects the first visible entry --- verified against
        // the drawn frame, not the formula, so a ratatui scroll change
        // breaks here first.
        let row = lines
            .iter()
            .position(|line| line.contains("file00.py"))
            .expect("the first entry is drawn");
        click(&mut app, 2, row as u16);
        let browser = app.browser.as_ref().unwrap();
        assert_eq!(app.focus, Focus::FilesLocal);
        assert_eq!(browser.focus, Side::Local);
        assert_eq!(
            browser
                .local_entries
                .get(browser.local_cursor)
                .map(|e| e.name.as_str()),
            Some("file00.py")
        );

        // A scrolled list maps through the same offset the renderer
        // settled on: jump to the end, redraw, click the top visible row.
        let browser = app.browser.as_mut().unwrap();
        browser.local_cursor = browser.local_entries.len() - 1;
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("file15.py"))
            .expect("the list scrolled so file15 leads");
        click(&mut app, 2, row as u16);
        let browser = app.browser.as_ref().unwrap();
        assert_eq!(
            browser
                .local_entries
                .get(browser.local_cursor)
                .map(|e| e.name.as_str()),
            Some("file15.py"),
            "the top visible row is the entry the click selected"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_below_a_short_list_moves_nothing() {
        let root = project_dir("short", 2);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.browser = Some(Browser::new(&root));
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("file00.py"))
            .unwrap();
        // Click well below the two-entry list: still inside the pane's
        // inner area, but on a row no entry occupies.
        click(&mut app, 2, row as u16 + 6);
        let browser = app.browser.as_ref().unwrap();
        assert_eq!(browser.local_cursor, 0, "the cursor did not move");
        assert_eq!(app.focus, Focus::FilesLocal, "but the pane took focus");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_the_workspace_pane_selects_its_row() {
        let root = project_dir("ws", 3);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        assert!(app.workspace.is_some(), "a build backend gets the pane");
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("CMakeLists.txt"))
            .expect("the listing is drawn");
        click(&mut app, 2, row as u16);
        assert_eq!(app.focus, Focus::Workspace);
        let panel = app.workspace.as_ref().unwrap();
        assert!(panel.visible_files()[panel.files_cursor].name == "CMakeLists.txt");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_the_environment_pane_selects_its_question() {
        let root = project_dir("env", 0);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("Project path"))
            .expect("MicroPython asks for the project path");
        assert_eq!(app.project_cursor, 0, "the pane starts on its first row");
        click(&mut app, 2, row as u16);
        assert_eq!(app.focus, Focus::Project);
        assert_eq!(app.project_cursor, 1, "the clicked row took the cursor");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_row3_focuses_the_log_pane_and_the_wheel_scrolls_it() {
        let root = project_dir("logs", 0);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        for i in 0..40 {
            app.logs.info(format!("entry {i}"));
        }
        let lines = render(&mut app, 100, 40);
        // Somewhere inside row 3: the log pane's body (the last content
        // rows of the frame, above the footer).
        let row = (lines.len() - 3) as u16;
        click(&mut app, 2, row);
        assert_eq!(app.focus, Focus::Logs);
        assert!(app.logs.is_following());
        wheel(&mut app, -1, 2, row);
        assert!(
            !app.logs.is_following(),
            "a wheel step over row 3 scrolls the log"
        );
        wheel(&mut app, 1, 2, row);
        assert!(app.logs.is_following(), "and back to the tail");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gestures_under_a_modal_or_before_a_frame_do_nothing() {
        let root = project_dir("modal", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.browser = Some(Browser::new(&root));
        render(&mut app, 100, 40);
        // An overlay owns the screen: the click must not reach the panes.
        let before = app.focus;
        app.overlay = Some(crate::app::Overlay::Help {
            filter: String::new(),
            filtering: false,
            selected: 0,
        });
        click(&mut app, 2, 8);
        assert_eq!(app.focus, before, "focus stayed where it was");
        app.overlay = None;
        // The shortcuts overlay owns it the same way.
        app.shortcuts_overlay_active = true;
        click(&mut app, 2, 8);
        assert_eq!(app.focus, before);
        app.shortcuts_overlay_active = false;
        // And with no published frame there is no geometry to click.
        app.frame_area = None;
        click(&mut app, 2, 8);
        assert_eq!(app.focus, before);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The screen column of `label`'s first char in a drawn line: the
    /// border rules are multi-byte UTF-8, so a *byte* offset from `find`
    /// is not the column a terminal click reports.
    fn column_of(line: &str, label: &str) -> Option<u16> {
        line.find(label)
            .map(|byte| line[..byte].chars().count() as u16)
    }

    #[test]
    fn a_click_on_a_log_tab_switches_the_strip() {
        let root = project_dir("tabs", 0);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        let lines = render(&mut app, 100, 40);
        // The strip rides row 3's top border; find its row by the Monitor
        // label every monitor-capable backend draws.
        let row = lines
            .iter()
            .position(|line| line.contains("Monitor"))
            .expect("the strip names its tabs") as u16;
        // MicroPython has the Monitor capability, so the strip draws
        // Log • Monitor • Terminal; click somewhere inside Monitor's
        // label range (after Log's, which comes first).
        let strip_line = &lines[row as usize];
        let monitor_col = column_of(strip_line, "Monitor").expect("Monitor drawn");
        assert_eq!(app.log_tab, LogTab::Log);
        click(&mut app, monitor_col, row);
        assert_eq!(
            app.log_tab,
            LogTab::Monitor,
            "the click switched to the tab it landed on"
        );
        // And back by clicking Log.
        let log_col = column_of(strip_line, "Log").expect("Log drawn");
        click(&mut app, log_col, row);
        assert_eq!(app.log_tab, LogTab::Log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_the_device_strip_flips_the_tab() {
        let root = project_dir("devtab", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.browser = Some(Browser::new(&root));
        // A flash-capable backend needs the panel before the strip exists;
        // `show_device_actions_tab` is what `x` runs --- the click under
        // test is the strip's own switch onto Files.
        app.show_device_actions_tab();
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("Device Files"))
            .expect("the strip names its tabs") as u16;
        let strip_line = &lines[row as usize];
        let files_col = column_of(strip_line, "Device Files").unwrap();
        assert!(app.device_actions_tab_active());
        click(&mut app, files_col, row);
        assert!(
            !app.device_actions_tab_active(),
            "the click flipped the strip to the files tab"
        );
        // And the strip's other label flips back.
        let actions_col = column_of(strip_line, "Actions").unwrap();
        click(&mut app, actions_col, row);
        assert!(app.device_actions_tab_active());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_a_build_button_selects_and_presses_it() {
        let root = project_dir("buildbtn", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        assert!(app.build.is_some(), "a build backend gets the panel");
        let lines = render(&mut app, 100, 40);
        // `Install Zephyr` is the stack's leading row for an unresolved
        // workspace and its action opens the install-directory picker ---
        // observable without hardware, a board, or a spawned process.
        let row = lines
            .iter()
            .position(|line| line.contains("Install Zephyr"))
            .expect("the stack draws Install Zephyr") as u16;
        let col = column_of(&lines[row as usize], "Install Zephyr").unwrap();
        click(&mut app, col, row);
        assert_eq!(app.focus, Focus::Build);
        let panel = app.build.as_ref().unwrap();
        let caps = app.manager.capabilities();
        assert_eq!(
            panel.actions(&caps)[panel.cursor],
            BuildAction::InstallZephyr,
            "the clicked row took the cursor"
        );
        assert!(
            app.overlay.is_some(),
            "the click ran the button, not just its selection"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_between_buttons_moves_nothing() {
        let root = project_dir("divider", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("Install Zephyr"))
            .unwrap() as u16;
        // The row between two button rows is the stack's divider; click
        // inside the build pane (its label's column).
        let col = column_of(&lines[row as usize], "Install Zephyr").unwrap();
        click(&mut app, col, row + 1);
        assert_eq!(app.focus, Focus::Build, "the pane still took focus");
        assert!(app.overlay.is_none(), "nothing ran");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_a_confirm_button_answers_the_dialog() {
        let root = project_dir("confirm", 2);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::ConfirmDelete {
            side: Side::Local,
            name: "file01.py".into(),
            is_dir: false,
            confirm: false,
        });
        let lines = render(&mut app, 100, 40);
        // The buttons' own row: the row carrying the No/Yes labels.
        let button_row = lines
            .iter()
            .position(|l| l.contains("No") && l.contains("Yes"))
            .expect("the dialog draws its buttons") as u16;
        let no = column_of(&lines[button_row as usize], "No").unwrap();
        let yes = column_of(&lines[button_row as usize], "Yes").unwrap();
        // `No` closes the dialog and keeps the file.
        click(&mut app, no, button_row);
        assert!(app.overlay.is_none(), "the dialog answered");
        assert!(root.join("file01.py").exists(), "No declined the delete");
        // Reopen and answer `Yes` on the drawn button: the file goes.
        app.overlay = Some(crate::app::Overlay::ConfirmDelete {
            side: Side::Local,
            name: "file01.py".into(),
            is_dir: false,
            confirm: false,
        });
        render(&mut app, 100, 40);
        click(&mut app, yes, button_row);
        assert!(app.overlay.is_none());
        assert!(!root.join("file01.py").exists(), "Yes ran the delete");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_beside_a_destructive_confirm_ignores_the_dialog() {
        let root = project_dir("destructive", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.overlay = Some(crate::app::Overlay::ConfirmBuild {
            action: BuildAction::Flash,
            confirm: false,
        });
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("west flash"))
            .expect("the destructive dialog quotes the command") as u16;
        // A click on the message area (not a button) is not an answer ---
        // dismissing by mis-click would be cheaper than Esc for the wrong
        // question.
        click(&mut app, 50, row);
        assert!(
            app.overlay.is_some(),
            "the dialog stays until a button is clicked"
        );
        let button_row = lines
            .iter()
            .position(|l| l.contains("No") && l.contains("Yes"))
            .unwrap() as u16;
        let no = column_of(&lines[button_row as usize], "No").unwrap();
        click(&mut app, no, button_row);
        assert!(app.overlay.is_none(), "the button itself answers");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_a_stacked_overlay_button_presses_it() {
        let root = project_dir("zactions", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.overlay = Some(crate::app::Overlay::ZephyrActions { selected: 0 });
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("Update Zephyr"))
            .expect("the stack draws its leading button") as u16;
        click(&mut app, 5, row);
        assert!(
            matches!(app.overlay, Some(crate::app::Overlay::ConfirmBuild { .. })),
            "the click pressed the button, which opened its confirm"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The Zephyr Actions stack draws two rows per button (label + the
    /// command's description) with dividers between pairs --- pinned row by
    /// row, label AND description, so a change to the stack grammar breaks
    /// here before it misreads a click anywhere else.
    #[test]
    fn zephyr_actions_clicks_read_label_and_description_rows() {
        for (label, pressed) in [
            ("Update Zephyr", 0),
            ("west update", 0),
            ("Add SDK toolchains", 1),
            ("extends the installed bundle", 1),
            ("Dashboard", 2),
            ("west build -t dashboard", 2),
        ] {
            let root = project_dir("za", 1);
            let mut app = app_with_backend(BackendKind::Zephyr, &root);
            app.overlay = Some(crate::app::Overlay::ZephyrActions { selected: 0 });
            let lines = render(&mut app, 100, 40);
            let row = lines
                .iter()
                .position(|l| l.contains(label))
                .unwrap_or_else(|| {
                    panic!(
                        "row {label:?} is drawn:
{}",
                        lines.join(
                            "
"
                        )
                    )
                }) as u16;
            let before = app.logs.len();
            click(&mut app, 20, row);
            let overlay = app.overlay.clone();
            match pressed {
                0 => assert!(
                    matches!(overlay, Some(crate::app::Overlay::ConfirmBuild { .. })),
                    "{label}: Update asks its confirm"
                ),
                // The test app resolves no workspace, so the picker's door
                // answers the way it does for the keyboard: a logged
                // warning, the menu staying put.
                1 => assert!(
                    matches!(overlay, Some(crate::app::Overlay::ZephyrActions { .. }))
                        && app.logs.len() > before,
                    "{label}: Add SDK answers through its own gate"
                ),
                _ => assert!(
                    app.build.as_ref().is_some_and(|panel| panel.is_busy())
                        || app.log_tab == LogTab::Monitor,
                    "{label}: Dashboard starts through the panel"
                ),
            }
            let _ = std::fs::remove_dir_all(&root);
        }
        // The dividers between pairs are not buttons.
        for divider_text in ["rewrites every checkout", "no full re-download"] {
            let root = project_dir("zadiv", 1);
            let mut app = app_with_backend(BackendKind::Zephyr, &root);
            app.overlay = Some(crate::app::Overlay::ZephyrActions { selected: 0 });
            let lines = render(&mut app, 100, 40);
            let row = lines.iter().position(|l| l.contains(divider_text)).unwrap() as u16;
            // The divider sits between description and the next label.
            click(&mut app, 20, row + 1);
            assert!(
                matches!(app.overlay, Some(crate::app::Overlay::ZephyrActions { .. })),
                "a divider row is not a button"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn a_click_on_a_picker_row_selects_without_activating() {
        let root = project_dir("picker", 0);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::ThemePicker { selected: 0 });
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("Tokyo Night"))
            .expect("the theme picker lists the themes") as u16;
        click(&mut app, 5, row);
        assert!(
            matches!(
                &app.overlay,
                Some(crate::app::Overlay::ThemePicker { selected, .. }) if *selected > 0
            ),
            "the click moved the picker's cursor"
        );
        assert!(
            matches!(&app.overlay, Some(crate::app::Overlay::ThemePicker { .. })),
            "selection alone does not apply a theme"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The online-firmware search is where the pointer was dead: the flash
    /// view is a dialog over the dashboard, and its screens need the same
    /// click grammar their keys have. With the curl fake feeding the
    /// session, a board row click *picks* the board (the fetch that moves
    /// the dialog to the firmware list) and a firmware row click asks to
    /// download --- here landing on the overwrite confirm a prepared
    /// local file provides, which keeps the whole flow offline.
    #[test]
    fn online_firmware_rows_answer_clicks() {
        use crate::backend::micropython::firmware::{BoardCandidate, FirmwareFile};

        let root = project_dir("online", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.show_device_actions_tab();
        let curl = format!("{}/tests/fixtures/bin/curl", env!("CARGO_MANIFEST_DIR"));
        let flash = app.flash.as_mut().unwrap();
        flash.set_curl_tool_path(&curl);
        flash.online_boards = vec![BoardCandidate {
            id: "ESP32_GENERIC".into(),
            product: "ESP32".into(),
            vendor: "Espressif".into(),
        }];
        flash.screen = FlashScreen::OnlineBoards;
        app.view = View::Flash;

        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("ESP32_GENERIC"))
            .expect("the boards table draws the board") as u16;
        click(&mut app, 20, row);
        let flash = app.flash.as_ref().unwrap();
        assert_eq!(
            flash.screen,
            FlashScreen::OnlineFirmware,
            "clicking a board row picks it, like Enter"
        );

        // The firmware list: clicking a build whose destination file
        // already exists lands on the overwrite confirm (`SPEC.md` §15).
        std::fs::create_dir_all(root.join("firmware")).unwrap();
        std::fs::write(root.join("firmware/v1.28.0.bin"), "old").unwrap();
        let flash = app.flash.as_mut().unwrap();
        flash.online_firmware = vec![FirmwareFile {
            label: "v1.28.0 (2026-04-06) .bin".into(),
            version: "v1.28.0".into(),
            date: "2026-04-06".into(),
            variant: String::new(),
            url: "https://example.com/v1.28.0.bin".into(),
            kind: crate::backend::micropython::firmware::FirmwareKind::Bin,
        }];
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("v1.28.0"))
            .expect("the firmware list draws the build") as u16;
        click(&mut app, 20, row);
        assert!(
            matches!(app.overlay, Some(Overlay::ConfirmDownloadOverwrite { .. })),
            "clicking a firmware build goes through the download confirm"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_double_click_opens_what_enter_opens() {
        let root = project_dir("dblclick", 3);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.browser = Some(Browser::new(&root));
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("file01.py"))
            .expect("an entry row is drawn") as u16;
        // First click: selection only.
        click(&mut app, 2, row);
        let browser = app.browser.as_ref().unwrap();
        let selected = browser.local_entries[browser.local_cursor].name.clone();
        assert_eq!(selected, "file01.py");
        assert!(app.overlay.is_none(), "one click selects, nothing more");
        // Second click on the same row, immediately: Enter --- the entry's
        // action menu, in the browser's grammar.
        click(&mut app, 2, row);
        assert!(
            matches!(app.overlay, Some(Overlay::FileActions { .. })),
            "a double-click opens the entry's menu, like Enter"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_an_environment_row_opens_its_dialog() {
        let root = project_dir("envrow", 0);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("Projects base"))
            .expect("the checklist draws the projects-base row") as u16;
        assert!(app.overlay.is_none());
        click(&mut app, 2, row);
        assert_eq!(app.focus, Focus::Project);
        assert!(
            matches!(app.overlay, Some(Overlay::DirPicker { .. })),
            "a click on a checklist row opens the row's dialog, like Enter"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The merged `Board · Shield` row carries two dialogs, and the click
    /// must open the one under the pointer --- not whichever half `←`/`→`
    /// last selected. Pinned against the drawn row: the board half ends
    /// where the ` · Shield: ` separator begins.
    /// A docs picker's click lands through the shared geometry: a row of
    /// the west list selects it and hands the list the keyboard, a click
    /// on the details pane hands it the keyboard instead (`Tab`'s mouse
    /// side).
    #[test]
    fn a_docs_picker_click_selects_and_the_details_takes_the_keyboard() {
        let root = project_dir("docspick", 0);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.build.as_mut().unwrap().set_tool_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/bin/west"
        ));
        app.open_board_picker();

        // Drain the background `west boards` fetch to its list.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            for event in app.processes.drain() {
                app.handle(AppEvent::Process(event));
            }
            if matches!(
                app.build.as_ref().unwrap().boards.state,
                crate::build::ListState::Loaded(_)
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the fake west boards never finished"
            );
        }

        // The row click: pinned against the drawn row, through its own
        // column (byte offsets are not columns).
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("rpi_pico/rp2040"))
            .expect("a board row is drawn");
        let col = column_of(&lines[row], "rpi_pico").unwrap();
        click(&mut app, col, row as u16);
        assert!(
            matches!(
                app.overlay,
                Some(Overlay::BoardPicker {
                    selected: 4,
                    focus: DocsFocus::List,
                    ..
                })
            ),
            "the row click selected the drawn row and kept the list's keyboard"
        );

        // The details pane's own frame takes the keyboard.
        let title = lines
            .iter()
            .position(|line| line.contains("Details"))
            .expect("the details pane is drawn");
        let col = column_of(&lines[title], "Details").unwrap();
        click(&mut app, col, title as u16);
        assert!(
            matches!(
                app.overlay,
                Some(Overlay::BoardPicker {
                    focus: DocsFocus::Details,
                    ..
                })
            ),
            "a click on the details pane hands it the keyboard"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn board_shield_clicks_open_the_half_they_land_on() {
        let root = project_dir("seg", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        // Leave the segment cursor on the *shield* half: a click on the
        // board half must still open the board picker.
        app.board_segment = false;

        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("· Shield:"))
            .expect("the merged row is drawn") as u16;
        let shield_col = column_of(&lines[row as usize], "· Shield:").unwrap();

        // The board half: anywhere left of the separator (the leading
        // label column included --- the row is one target).
        click(&mut app, 2, row);
        assert!(
            matches!(app.overlay, Some(Overlay::BoardPicker { .. })),
            "a click on the board half opens the board picker, whatever the last segment was"
        );

        // And the shield half, with the segment cursor parked on board.
        app.overlay = None;
        app.board_segment = true;
        render(&mut app, 100, 40);
        click(&mut app, shield_col + 4, row);
        assert!(
            matches!(app.overlay, Some(Overlay::ShieldPicker { .. })),
            "a click past the separator opens the shield picker"
        );
        assert!(!app.board_segment, "the click set the segment it landed on");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The MAC row's click queues the clipboard write (the binary's loop
    /// turns it into the terminal's OSC 52) --- pinned against the drawn
    /// row, and only that row: the pane's other facts stay inert.
    #[test]
    fn clicking_the_mac_row_copies_it() {
        let root = project_dir("mac", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        let mac = "24:6F:28:AA:BB:CC".to_string();
        app.show_device_actions_tab();
        app.flash.as_mut().unwrap().details.mac = Some(mac.clone());

        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("MAC:"))
            .expect("the MAC row is drawn") as u16;
        assert!(app.take_clipboard_request().is_none());
        // The MAC text lives in the *device* pane (the frame's right
        // half); clicking its own column keeps the aim honest.
        let mac_col = column_of(&lines[row as usize], "24:6F:28").unwrap();
        click(&mut app, mac_col, row);
        assert_eq!(
            app.take_clipboard_request(),
            Some(mac),
            "the MAC row's click queues exactly the MAC"
        );

        // A neighbouring row (the firmware identity) is not a copy target.
        render(&mut app, 100, 40);
        let other = lines.iter().position(|l| l.contains("Firmware:")).unwrap() as u16;
        click(&mut app, mac_col, other);
        assert!(
            app.take_clipboard_request().is_none(),
            "only the MAC row copies"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_click_on_the_header_project_name_switches_projects() {
        let root = project_dir("header", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        // The header names a project only once the project question has an
        // answer; the mpy project pick is the cheap way to one.
        app.set_mpy_project(root.clone());
        let lines = render(&mut app, 100, 40);
        let header = &lines[0];
        assert!(header.contains("Project"), "the header names the project");
        let col = column_of(header, &root.file_name().unwrap().to_string_lossy())
            .expect("the name is drawn");
        click(&mut app, col, 0);
        // The scan the setup spawned still counts as a running command, so
        // shift+P asks before switching --- the same question the keyboard
        // gets. Answering Yes on the drawn button completes the switch.
        assert!(
            matches!(app.overlay, Some(Overlay::ConfirmSwitchProject { .. })),
            "clicking the project name is shift+P: it asks while a command runs"
        );
        let lines = render(&mut app, 100, 40);
        let button_row = lines
            .iter()
            .position(|l| l.contains("No") && l.contains("Yes"))
            .expect("the confirm draws its buttons") as u16;
        let yes = column_of(&lines[button_row as usize], "Yes").unwrap();
        click(&mut app, yes, button_row);
        assert!(
            app.switch_requested(),
            "answering the switch question completes it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
