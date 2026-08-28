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
//! step over a cursor-walked list steps that list's cursor one row
//! (clamped, never moving focus --- the board picker's wheel grammar),
//! and over row 3 scrolls the pane under it.
//!
//! A click never does more than the keyboard can: rows, buttons and tabs
//! all land through the same handlers `Enter` and `←/→` reach, so a click
//! can select, activate and switch --- and nothing more. A destructive
//! confirm is no exception once the overlay stage gives it a target it can
//! name: a click on the drawn Yes/No button synthesizes exactly the `y`/`n`
//! keypress `on_overlay_key` would answer it with (`on_overlay_mouse`),
//! never a meaning of the click's own.

use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Flex, Layout, Rect};

use crate::app::{
    App, DevicePaneTab, DocsFocus, FileAction, Focus, LogTab, Overlay, ProjectRow, ThemeChoice,
    View,
};
use crate::backend::BackendKind;
use crate::browser::{PaneState, Side};
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
            self.click_device_info(point, areas.device);
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

    /// The Device Info pane: focuses it, like every other pane. Before
    /// anything is identified (`on_device_info_key`'s own empty check --- no
    /// `flash` yet, or a `flash` with nothing read), a double click is
    /// `Enter`'s twin here too, the same identification question `ctrl+r`
    /// opens --- the pane has no row to select while it is empty, so the
    /// double click's target is the pane itself (`index` 0). Once something
    /// is read, the one clickable fact is the MAC row, copied the same way
    /// `Enter` on the focused pane does.
    fn click_device_info(&mut self, point: (u16, u16), rect: Rect) {
        self.focus = Focus::DeviceInfo;
        let unidentified = self
            .flash
            .as_ref()
            .is_none_or(|flash| flash.details.is_empty());
        if unidentified {
            self.maybe_double_click(Focus::DeviceInfo, 0);
            return;
        }
        let mac = self
            .flash
            .as_ref()
            .and_then(|flash| flash.details.mac.clone());
        if let Some(mac) = mac
            && let Some(row) =
                crate::ui::device_mac_row(self, rect.width.saturating_sub(2) as usize)
            && inner_row(point, rect, 0, 0) == Some(row)
        {
            self.copy_to_clipboard("MAC", mac);
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
        if let Some(index) = drawn_list_row(
            point,
            rect,
            browser.local_offset,
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
        if let Some(index) = drawn_list_row(
            point,
            rect,
            browser.device_offset,
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
        let hit = drawn_list_row(
            point,
            rect,
            panel.files_offset,
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
        if let Some(tab) = strip_tab(point, rect, &self.device_strip_tabs()) {
            // A click on the strip lands on the tab it names, the same
            // target the chord flips to --- not merely the opposite of
            // whichever tab is active, or a click on the already-active
            // label would flip away from it.
            match tab {
                DevicePaneTab::Files => self.device_pane_tab = DevicePaneTab::Files,
                DevicePaneTab::Actions => self.show_device_actions_tab(),
            }
            return;
        }
        if self.device_actions_tab_active() {
            self.click_flash_stack(point, rect);
        } else {
            self.click_device(point, rect);
        }
    }

    /// A wheel step over a cursor-walked list steps that list's cursor one
    /// row, clamped at the ends --- the board picker's own wheel grammar
    /// (`on_overlay_wheel`), and like that one it never moves focus:
    /// scrolling past a pane is not pointing at it. Row 3 keeps its own
    /// answer: the wheel scrolls the active tab's content, the way
    /// shift+pgup would.
    fn wheel(&mut self, direction: isize, point: (u16, u16), areas: &layout::DashboardAreas) {
        if self.wheel_steps_list(direction, point, areas) {
            return;
        }
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

    /// The wheel over row 1's checklist and row 2's file lists: the pane
    /// under the pointer has its cursor stepped (`stepped`), nothing else.
    /// Answers whether the point landed on one of those panes, so an
    /// untouched wheel still reaches row 3's scroll.
    fn wheel_steps_list(
        &mut self,
        direction: isize,
        point: (u16, u16),
        areas: &layout::DashboardAreas,
    ) -> bool {
        // Row 1's Environment checklist: a fixed row set, so a notch is a
        // row step (`project_rows` answers the row count).
        if contains(areas.project, point) {
            let len = self.project_rows().len();
            self.project_cursor = stepped(self.project_cursor, len, direction);
            return true;
        }
        match &areas.row2 {
            Row2::WorkspaceBuild { workspace, .. } if contains(*workspace, point) => {
                // An error paragraph owns the pane; there are no rows to
                // walk (the click's own rule).
                if let Some(panel) = self.workspace.as_mut()
                    && panel.files_error.is_none()
                {
                    let len = panel.files_row_count();
                    panel.files_cursor = stepped(panel.files_cursor, len, direction);
                }
                true
            }
            Row2::Browser(row) if contains(row.local, point) => {
                if let Some(browser) = self.browser.as_mut() {
                    let len = browser.visible_local().len();
                    browser.local_cursor = stepped(browser.local_cursor, len, direction);
                }
                true
            }
            Row2::Browser(row)
                if contains(row.right, point) && matches!(row.right_kind, RightKind::Device) =>
            {
                // The actions tab is a button stack, not a listing, and a
                // non-Ready pane draws no rows --- nothing to step either
                // way (the click's own gates).
                if !self.device_actions_tab_active()
                    && let Some(browser) = self.browser.as_mut()
                    && matches!(browser.device_state, PaneState::Ready)
                {
                    let len = browser.visible_device().len();
                    browser.device_cursor = stepped(browser.device_cursor, len, direction);
                }
                true
            }
            _ => false,
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
        // The stack starts at the pane's inner top row. None of this
        // pane's buttons carry a `.detail()` line (`SPEC.md` §15: the rows
        // stay bare), so a bare placeholder per action has the same row
        // shape `ui::button::render_stack` actually drew --- `button_at_row`
        // is the one place that shape is turned into a button index, kept
        // in step with [`crate::ui::button::stack_height`].
        let Some(row) = point.1.checked_sub(rect.y + 1) else {
            return;
        };
        let placeholders: Vec<crate::ui::Button> =
            (0..mains).map(|_| crate::ui::Button::new("")).collect();
        if let Some(index) = crate::ui::button_at_row(&placeholders, row) {
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
        // Same bare-button row shape as the build pane's stack, and the
        // same shared `button_at_row` --- see `click_build_stack`.
        let Some(row) = point.1.checked_sub(rect.y + 1) else {
            return;
        };
        let placeholders: Vec<crate::ui::Button> =
            (0..mains).map(|_| crate::ui::Button::new("")).collect();
        if let Some(index) = crate::ui::button_at_row(&placeholders, row) {
            let action = actions[index];
            self.flash.as_mut().unwrap().pane_cursor = index;
            self.run_flash_pane_action(action);
        }
    }

    /// The titles a pane's tab strip draws, in draw order --- the two
    /// strips' label sequences, kept beside the hit-tester that maps a
    /// click onto one of them. Built with the session's own icon set, so
    /// the widths match the strip the frame drew (`none` drops the glyph
    /// and its gap, changing the ranges); the leading stop number, when the
    /// pane carries one, is prepended here the same way the renderer
    /// prepends it (`files::draw_device_tabs`, `panels::draw_log_tabs`).
    fn log_strip_tabs(&self) -> Vec<(LogTab, String)> {
        let icons = self.icon_set();
        let number = self.pane_number(Focus::Logs);
        self.available_log_tabs()
            .into_iter()
            .enumerate()
            .map(|(position, tab)| {
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
                let title = crate::ui::pane_title(glyph, title);
                let title = match (position == 0, number) {
                    (true, Some(number)) => format!("{number} {title}"),
                    _ => title,
                };
                (tab, title)
            })
            .collect()
    }

    fn device_strip_tabs(&self) -> Vec<(DevicePaneTab, String)> {
        let icons = self.icon_set();
        let number = self.pane_number(Focus::FilesDevice);
        [
            (DevicePaneTab::Actions, icons.bolt(), "Actions"),
            (DevicePaneTab::Files, icons.folder(), "Device Files"),
        ]
        .into_iter()
        .enumerate()
        .map(|(position, (tab, glyph, title))| {
            let title = crate::ui::pane_title(glyph, title);
            let title = match (position == 0, number) {
                (true, Some(number)) => format!("{number} {title}"),
                _ => title,
            };
            (tab, title)
        })
        .collect()
    }

    /// The open overlay's own click handling, reached instead of the
    /// dashboard's while a modal owns the screen (the same standing
    /// `on_overlay_key` has). A click outside the dialog's drawn rect
    /// closes it exactly like `Esc` (synthesized into `on_overlay_key`, so
    /// every per-variant special case applies unchanged) --- a mis-click
    /// beside a destructive confirm is exactly as safe as pressing `Esc`
    /// for the same question, never a silent no-op.
    ///
    /// The grammar, one rule per shape: a confirm's `No`/`Yes` buttons
    /// answer the question directly (a click on a drawn button is as
    /// explicit as `y`/`n` --- synthesized as exactly those keys, so every
    /// per-variant accept/decline path is the keyboard's own); a picker's
    /// rows select (`Enter` stays the activation); a stacked-button menu
    /// (Zephyr Actions, the installer's footer button) selects and presses
    /// through `Enter`; the SDK checklist's rows toggle, the way a checkbox
    /// click means `Space`. Input dialogs, the viewer and the help window
    /// have no click surface of their own --- inside their popup a click
    /// does nothing (their one meaningful gesture is typing), but outside
    /// it still closes them, the same rule as everything else here.
    /// The wheel is the one non-click gesture an overlay answers, and only
    /// the docs pickers have panes worth scrolling ([`Self::on_overlay_wheel`]).
    fn on_overlay_mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {}
            MouseEventKind::ScrollUp => return self.on_overlay_wheel(-1, event),
            MouseEventKind::ScrollDown => return self.on_overlay_wheel(1, event),
            _ => return,
        }
        let Some(frame) = self.frame_area else { return };
        let point = (event.column, event.row);
        let Some(overlay) = self.overlay.as_ref() else {
            return;
        };
        // A click outside the dialog closes it, exactly like `Esc` ---
        // reusing the keyboard's own handler is what makes every
        // per-variant special case (Help's filtering step-back, the
        // interrupt/remove-package confirms' "return to Packages", the
        // installer's busy guard) apply automatically, with nothing
        // special-cased here.
        let rect = layout::overlay_popup(self, overlay, frame);
        if !contains(rect, point) {
            self.overlay_key(KeyCode::Esc);
            return;
        }
        match overlay {
            // ---- confirms: the drawn No/Yes buttons answer -------------
            Overlay::Confirm { .. }
            | Overlay::ConfirmBuild { .. }
            | Overlay::ConfirmRestartDevice { .. }
            | Overlay::ConfirmSwitchProject { .. }
            | Overlay::ConfirmEraseForMicroPython { .. }
            | Overlay::ConfirmIdentifyDevice { .. }
            | Overlay::ConfirmInterruptDevice { .. }
            | Overlay::ConfirmDelete { .. }
            | Overlay::ConfirmDownloadOverwrite { .. }
            | Overlay::ConfirmUpload { .. }
            | Overlay::SyncPreview { .. }
            | Overlay::ConfirmInstallHere { .. }
            // Drawn by the same `draw_destructive` as `ConfirmBuild`, so
            // `confirm_buttons` locates its No/Yes the same way. It used to
            // sit outside this arm, which left the package manager's removal
            // dialog answering a click beside the box (cancel) but not one
            // on `Yes`.
            | Overlay::ConfirmRemovePackage { .. } => {
                let Some((no, yes)) = confirm_buttons(rect) else {
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
                    rect,
                    *selected,
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
                    rect,
                    *selected,
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
                    rect,
                    *selected,
                    len,
                    0,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::Packages => {
                // Picker grammar: a click selects, never activates ---
                // installing and removing stay behind `Enter`/`Del`, whose
                // gates the keyboard owns. A click on either pane also
                // hands it the keyboard, the docs pickers' own rule.
                let areas = crate::ui::layout::packages(frame);
                if areas.details.contains(point.into()) {
                    self.packages.focus = super::DocsFocus::Details;
                    return;
                }
                if !areas.list.contains(point.into()) {
                    return;
                }
                self.packages.focus = super::DocsFocus::List;
                let len = self.package_rows().len();
                // `list_row` takes the *bordered* rect and subtracts the
                // two rules itself.
                if let Some(index) = list_row(point, areas.list, self.packages.selected, len, 0, 0)
                {
                    self.packages.selected = index;
                    self.packages.scroll = 0;
                }
            }
            Overlay::BuildDashboard => {
                // The strip first: it sits on the modal's top border, which
                // is inside the popup rect, so a click there would
                // otherwise fall through to the list test below.
                let areas = layout::build_dashboard(frame);
                let tabs = self.dashboard_strip_tabs();
                if let Some(tab) = strip_tab(point, areas.popup, &tabs) {
                    self.set_dashboard_tab(tab);
                    return;
                }
                // Picker grammar: a click selects, never activates ---
                // expanding stays behind `Enter`/`→`. A click on either
                // pane also hands it the keyboard, the docs pickers' rule.
                if contains(areas.details, point) {
                    self.set_dashboard_focus(DocsFocus::Details);
                    return;
                }
                if !contains(areas.list, point) {
                    return;
                }
                self.set_dashboard_focus(DocsFocus::List);
                let len = self.build_dashboard.rows().len();
                if let Some(index) =
                    dashboard_list_row(areas.list, point, self.dashboard_list_offset, len)
                {
                    self.set_dashboard_selection(index);
                }
            }
            Overlay::ProjectSetup { selected } => {
                let len = BackendKind::ALL.len();
                if let Some(index) = list_row(
                    point,
                    rect,
                    *selected,
                    len,
                    2,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::RestoreDeviceScript { selected, .. } => {
                // Three constant choices under a two-row message.
                if let Some(index) =
                    list_row(point, rect, *selected, 3, 2, 0)
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
                let is_text = crate::files::is_text_like(name);
                let actions = FileAction::for_entry(
                    *side,
                    *is_dir,
                    is_text,
                    *status,
                    self.manager.capabilities(),
                );
                let len = actions.len();
                if let Some(index) = list_row(
                    point,
                    rect,
                    *selected,
                    len,
                    0,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::DirPicker { path, selected, .. } => {
                let len = crate::workspace::dir_rows(path).0.len();
                if let Some(index) = list_row(
                    point,
                    rect,
                    *selected,
                    len,
                    1,
                    2,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::ProjectPicker { mpy, selected, .. } => {
                let mpy = *mpy;
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
                    rect,
                    *selected,
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
                    .map(|panel| panel.filtered_build_dirs(input).len())
                else {
                    return;
                };
                if let Some(index) = list_row(
                    point,
                    rect,
                    *selected,
                    len,
                    3,
                    0,
                ) {
                    self.set_overlay_selected(index);
                }
            }
            Overlay::BoardPicker { input, .. } => {
                // A click on the details pane hands it the keyboard (the
                // mouse's way of `Tab`); a click on the list side hands it
                // back and selects the row under the pointer.
                let areas = layout::docs_picker(frame);
                if contains(areas.details, point) {
                    self.set_docs_picker_focus(DocsFocus::Details);
                    return;
                }
                if !contains(areas.list, point) {
                    // Neither pane: the preview thumbnail or the frame
                    // around the modal --- selects nothing, and must not
                    // silently steal the keyboard back from Details.
                    return;
                }
                // Owned before the focus set releases `self.overlay`'s
                // borrow --- `input`/`selected` still name the overlay's
                // own fields either way, just no longer through it.
                let input = input.clone();
                self.set_docs_picker_focus(DocsFocus::List);
                let Some(len) = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_boards_count(&input))
                else {
                    return;
                };
                if let Some(index) = docs_list_row(frame, point, self.docs_list_offset, len) {
                    self.set_docs_picker_selection(index);
                }
            }
            Overlay::ShieldPicker { input, .. } => {
                // Same grammar as the board picker, over a list whose row 0
                // is the `(none)` row --- it is included in the count.
                let areas = layout::docs_picker(frame);
                if contains(areas.details, point) {
                    self.set_docs_picker_focus(DocsFocus::Details);
                    return;
                }
                if !contains(areas.list, point) {
                    // Neither pane: the preview thumbnail or the frame
                    // around the modal --- selects nothing, and must not
                    // silently steal the keyboard back from Details.
                    return;
                }
                let input = input.clone();
                self.set_docs_picker_focus(DocsFocus::List);
                let Some(len) = self
                    .build
                    .as_ref()
                    .map(|panel| 1 + panel.filtered_shields_count(&input))
                else {
                    return;
                };
                if let Some(index) = docs_list_row(frame, point, self.docs_list_offset, len) {
                    self.set_docs_picker_selection(index);
                }
            }

            // ---- checklist: a row click toggles, the checkbox meaning --
            Overlay::SdkToolchains { selected } => {
                let len = crate::install::steps::TOOLCHAINS.len();
                if let Some(index) = list_row(point, rect, *selected, len, 0, 1) {
                    self.set_overlay_selected(index);
                    self.overlay_key(KeyCode::Char(' '));
                }
            }

            // ---- stacked buttons: select and press ---------------------
            Overlay::ZephyrActions { .. } => {
                // `ZEPHYR_ACTIONS_COUNT` detailed (label + description)
                // buttons, the same row shape `draw_zephyr_actions` renders
                // --- `button_at_row` is the shared source of truth for
                // turning that shape into a button index (`click_build_stack`
                // and `click_flash_stack` follow the same rule for their own,
                // undetailed stacks).
                let placeholders: Vec<crate::ui::Button> = (0..crate::ui::ZEPHYR_ACTIONS_COUNT)
                    .map(|_| crate::ui::Button::new("").detail(""))
                    .collect();
                let Some(row) = point.1.checked_sub(rect.y + 1) else {
                    return;
                };
                if let Some(index) = crate::ui::button_at_row(&placeholders, row) {
                    self.set_overlay_selected(index);
                    self.overlay_key(KeyCode::Enter);
                }
            }
            Overlay::ZephyrInstall => {
                // The installer's footer button, pinned to the modal's
                // bottom rows on the right --- the same box `Enter` presses.
                let inner = Rect {
                    x: rect.x + 1,
                    y: rect.y + 1,
                    width: rect.width.saturating_sub(2),
                    height: rect.height.saturating_sub(2),
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

    /// A wheel step inside a docs picker (the board/shield pickers' modal)
    /// scrolls the pane under the pointer, the same split its arrows make
    /// once `Tab` hands them a side: a step over the west list walks its
    /// rows one per event (a notch is a nudge, not a page --- and a notch
    /// that reports several events still moves one row each, the way a
    /// list scrolls everywhere else), clamped at the ends (the home
    /// screen's wheel rule: the arrows wrap, a wheel that wraps feels like
    /// a bug) and the new row restarts the details pane from its top, like
    /// every cursor move; a step over the details scrolls its text a line
    /// at a time the way the row-3 wheel scrolls the log (the renderer
    /// clamps the tail, the same contract the arrows rely on). Focus never
    /// moves: scrolling past a pane is not pointing at it, the rule the
    /// dashboard's own wheel follows.
    fn on_overlay_wheel(&mut self, direction: isize, event: MouseEvent) {
        let Some(frame) = self.frame_area else { return };
        let point = (event.column, event.row);
        // The filtered list the keyboard walks; shields add the `(none)`
        // row that clears the pick. Owned before the write-back releases
        // `self.overlay`'s borrow.
        let (selected, scroll, len) = match self.overlay.as_ref() {
            Some(Overlay::BoardPicker {
                input,
                selected,
                scroll,
                ..
            }) => {
                let len = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_boards_count(input))
                    .unwrap_or(0);
                (*selected, *scroll, len)
            }
            Some(Overlay::ShieldPicker {
                input,
                selected,
                scroll,
                ..
            }) => {
                let len = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_shields_count(input) + 1)
                    .unwrap_or(1);
                (*selected, *scroll, len)
            }
            // The build dashboard steps its own list and scrolls its own
            // details; its state lives on `App`, not in the variant, so it
            // is answered here rather than through the tuple below.
            Some(Overlay::BuildDashboard) => {
                let areas = layout::build_dashboard(frame);
                if contains(areas.list, point) {
                    self.build_dashboard.move_cursor(direction as i32);
                } else if contains(areas.details, point) {
                    let pane = self.build_dashboard.pane_mut();
                    pane.scroll = if direction < 0 {
                        pane.scroll.saturating_sub(1)
                    } else {
                        pane.scroll.saturating_add(1)
                    };
                }
                return;
            }
            _ => return,
        };
        let areas = layout::docs_picker(frame);
        if contains(areas.list, point) {
            if len == 0 {
                return;
            }
            let moved = selected as isize + direction;
            let index = moved.clamp(0, len as isize - 1) as usize;
            self.set_docs_picker_selection(index);
        } else if contains(areas.details, point) {
            let moved = if direction < 0 {
                scroll.saturating_sub(1)
            } else {
                scroll.saturating_add(1)
            };
            if let Some(
                Overlay::BoardPicker { scroll, .. } | Overlay::ShieldPicker { scroll, .. },
            ) = &mut self.overlay
            {
                *scroll = moved;
            }
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
    /// only moves its field focus (its `Enter` opens a text edit). A click
    /// outside the dialog's popup closes it exactly like `Esc`, the same
    /// leading check `on_overlay_mouse` has.
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
        // A click outside the dialog closes it, exactly like `Esc` ---
        // `leave_flash_screen`'s own back-one-level behavior for the
        // Options/Online*/CustomUrl screens applies unchanged, since this
        // synthesizes the same key `on_flash_key` handles.
        if !contains(popup, point) {
            self.flash_key(KeyCode::Esc);
            return;
        }
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
    let row_in_list = (point.1 - list.y) as usize;
    if row_in_list < header {
        return None;
    }
    let offset = selected.saturating_sub(height - 1 - header);
    let index = offset + row_in_list - header;
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

/// The cursor after one wheel notch over a list: a single row step,
/// clamped at both ends, unchanged when the list has nothing to walk ---
/// the board picker's wheel arithmetic, shared by every list the dashboard
/// wheel steps.
fn stepped(cursor: usize, len: usize, direction: isize) -> usize {
    if len == 0 {
        return cursor;
    }
    (cursor as isize + direction).clamp(0, len as isize - 1) as usize
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
/// or on a row no entry occupies. For the *overlay* lists, which draw with
/// a fresh `ListState` every frame: the scroll offset ratatui settles on
/// is the minimal one that keeps `selected` visible --- a pure function of
/// the selection and the height, reproduced here. The dashboard's three
/// file panes no longer qualify (they seed the state from the previous
/// frame's offset, so a click on a visible row does not re-anchor the
/// view) and map through [`drawn_list_row`] instead.
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

/// Maps a click onto one of the three dashboard file lists through the
/// offset the pane actually drew --- the settled offset `render_list`
/// published on its previous frame (`WorkspacePanel::files_offset`,
/// `Browser::local_offset`/`device_offset`), the same contract
/// [`docs_list_row`] follows for the pickers. The lists seed their
/// `ListState` from that offset, so a click on a visible row must not
/// re-anchor the view --- which is what [`list_row`]'s recomputed
/// minimal-scroll offset would do.
fn drawn_list_row(
    point: (u16, u16),
    rect: Rect,
    offset: usize,
    len: usize,
    skip: u16,
    reserved: u16,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let row = inner_row(point, rect, skip, reserved)?;
    let index = offset + row;
    (index < len).then_some(index)
}

/// The `No`/`Yes` button rects of a `draw_confirm_dialog`-shaped modal ---
/// re-derived with the exact `Layout` calls the renderer uses (the same
/// `centered`, the same vertical message/buttons split, the same centered
/// `10/4/10` horizontal split), so a click lands on the box that was drawn
/// without a second opinion about rounding.
fn confirm_buttons(popup: Rect) -> Option<(Rect, Rect)> {
    if popup.width == 0 || popup.height == 0 {
        return None;
    }
    let height = popup.height;
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
/// geometry the frame drew ([`layout::docs_picker`]). The row is mapped
/// through the offset the frame actually settled on (`App::docs_list_offset`,
/// published by the renderer) --- not recomputed from the selection, which
/// would assume a bottom-anchored list and land the click below the row the
/// pointer rested on.
fn docs_list_row(area: Rect, point: (u16, u16), offset: usize, len: usize) -> Option<usize> {
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
    let index = offset + (point.1 - inner.y) as usize;
    (index < len).then_some(index)
}

/// A row of the build dashboard's list, mapped through the offset the frame
/// settled on --- [`docs_list_row`]'s rule, over that window's own geometry.
fn dashboard_list_row(pane: Rect, point: (u16, u16), offset: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let inner = Rect {
        x: pane.x + 1,
        y: pane.y + 1,
        width: pane.width.saturating_sub(2),
        height: pane.height.saturating_sub(2),
    };
    if !contains(inner, point) || inner.height == 0 {
        return None;
    }
    let index = offset + (point.1 - inner.y) as usize;
    (index < len).then_some(index)
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
    fn bare_list_row_never_selects_the_header_row() {
        // A table with a leading header row (the online-boards screen's
        // own shape, `header=1`): a click on the header itself must not
        // underflow the `usize` math and must select nothing, not the
        // first entry.
        let list = Rect::new(0, 5, 40, 6);
        assert_eq!(bare_list_row((3, list.y), list, 0, 3, 1), None);
        // The row right below the header still resolves normally.
        assert_eq!(bare_list_row((3, list.y + 1), list, 0, 3, 1), Some(0));
    }

    /// A click on a *visible* row of a scrolled list must not move the
    /// view: the pane seeds its `ListState` from the previous frame's
    /// settled offset, so selecting a row inside the window leaves the
    /// window alone --- where a fresh-state render re-anchored any
    /// selection past half the pane to the bottom edge, and the clicked
    /// row visibly jumped.
    #[test]
    fn a_click_on_a_visible_row_keeps_the_list_where_it_was() {
        let root = project_dir("nojump", 30);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.browser = Some(Browser::new(&root));
        // The cursor lands deep enough that the window is scrolled, and the
        // row to click sits mid-window --- exactly where a re-anchor to the
        // bottom edge would move it.
        app.browser.as_mut().unwrap().local_cursor = 20;
        let before = render(&mut app, 100, 40);
        let row = before
            .iter()
            .position(|line| line.contains("file17.py"))
            .expect("a mid-window row is drawn");
        let offset = app.browser.as_ref().unwrap().local_offset;
        assert!(offset > 0, "the list must be scrolled for the test to bite");

        click(&mut app, 2, row as u16);

        let after = render(&mut app, 100, 40);
        assert_eq!(
            after.iter().position(|line| line.contains("file17.py")),
            Some(row),
            "the clicked row must stay on its drawn line:\n{}",
            after.join("\n")
        );
        assert_eq!(
            app.browser.as_ref().unwrap().local_offset,
            offset,
            "the window did not move"
        );
        assert_eq!(
            app.browser
                .as_ref()
                .unwrap()
                .local_entries
                .get(app.browser.as_ref().unwrap().local_cursor)
                .map(|entry| entry.name.as_str()),
            Some("file17.py"),
            "the click still selects the row it landed on"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The workspace Files pane answers the same rule (its own seed and
    /// publish, its own hit-test path).
    #[test]
    fn a_click_on_a_visible_workspace_row_keeps_the_view() {
        let root = project_dir("wsnojump", 40);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        assert!(app.workspace.is_some(), "a build backend gets the pane");
        app.workspace.as_mut().unwrap().files_cursor = 30;
        let before = render(&mut app, 100, 40);
        let row = before
            .iter()
            .position(|line| line.contains("file25.py"))
            .expect("a mid-window row is drawn");
        let offset = app.workspace.as_ref().unwrap().files_offset;
        assert!(offset > 0, "the list must be scrolled for the test to bite");

        click(&mut app, 2, row as u16);

        let after = render(&mut app, 100, 40);
        assert_eq!(
            after.iter().position(|line| line.contains("file25.py")),
            Some(row),
            "the clicked row must stay on its drawn line:\n{}",
            after.join("\n")
        );
        assert_eq!(
            app.workspace.as_ref().unwrap().files_offset,
            offset,
            "the window did not move"
        );
        let panel = app.workspace.as_ref().unwrap();
        assert_eq!(
            panel
                .visible_files()
                .get(panel.files_cursor)
                .map(|e| e.name.as_str()),
            Some("file25.py"),
            "the click still selects the row it landed on"
        );
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

    /// What a modal guarantees is that the gesture never reaches the *panes*
    /// underneath --- not that it does nothing at all: a click outside the
    /// popup is `Esc`, so the first one below also dismisses Help. That is
    /// the policy, and irrelevant here (the overlay is replaced right
    /// after); the assertion is about the focus below.
    #[test]
    fn a_gesture_under_a_modal_or_before_a_frame_never_reaches_the_panes() {
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
    fn a_click_on_the_already_active_device_tab_stays_put() {
        let root = project_dir("devtab-active", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.browser = Some(Browser::new(&root));
        app.show_device_actions_tab();
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("Device Files"))
            .expect("the strip names its tabs") as u16;
        let strip_line = &lines[row as usize];
        let actions_col = column_of(strip_line, "Actions").unwrap();
        assert!(app.device_actions_tab_active());
        click(&mut app, actions_col, row);
        assert!(
            app.device_actions_tab_active(),
            "clicking the already-active label must not flip away from it"
        );
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

    /// The bug the shared [`layout::overlay_popup`] closed: the device
    /// picker draws a fixed 52x4 box carrying "No MicroPython device found."
    /// when nothing is plugged in, while the hit-testing sized it `64 x
    /// len + 2` unconditionally --- a 64x2 band that does not overlap the
    /// drawn box at all. A click on the message read as "outside" and
    /// dismissed the dialog the user had just clicked on.
    #[test]
    fn a_click_inside_the_empty_device_picker_does_not_dismiss_it() {
        let root = project_dir("emptypicker", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::DevicePicker { selected: 0 });
        let lines = render(&mut app, 100, 40);
        assert!(
            app.devices.devices().is_empty(),
            "the fixture /dev is empty, which is what makes this the empty branch"
        );
        let message = lines
            .iter()
            .position(|line| line.contains("No MicroPython device found."))
            .expect("the empty picker draws its message") as u16;
        // The drawn box's left rule, read off the frame rather than
        // recomputed --- the two rects differ by exactly this margin. The
        // message is a paragraph inside the block, so it starts one column
        // right of the rule. (Scanning the row for its first non-blank
        // would find the *dashboard's* border instead: the popup clears
        // only its own rect.)
        let box_left =
            column_of(&lines[message as usize], "No MicroPython device found.").unwrap() - 1;

        // The title rule, one row above the message: inside the drawn 52x4
        // box, outside the 64x2 band the old hit-testing computed.
        click(&mut app, box_left + 4, message - 1);
        assert!(
            matches!(app.overlay, Some(crate::app::Overlay::DevicePicker { .. })),
            "a click on the dialog's own title row keeps it open"
        );

        // And the mirror: two columns left of the box is plainly beside the
        // dialog, yet fell *inside* the wider band and was silently ignored.
        click(&mut app, box_left - 2, message);
        assert!(
            app.overlay.is_none(),
            "a click beside the drawn box dismisses it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// [`Overlay::ConfirmRemovePackage`] is drawn by the same
    /// `draw_destructive` as `ConfirmBuild`, but sat outside the arm that
    /// maps a click onto the drawn No/Yes --- so clicking `Yes` did nothing
    /// while clicking one column beside the box cancelled.
    #[test]
    fn a_click_answers_the_package_removal_confirm() {
        let root = project_dir("removepkg", 1);
        for confirm in [false, true] {
            let mut app = app_with_backend(BackendKind::MicroPython, &root);
            app.overlay = Some(crate::app::Overlay::ConfirmRemovePackage {
                name: "umqtt.simple".into(),
                targets: Vec::new(),
                declared: true,
                confirm: false,
            });
            let lines = render(&mut app, 100, 40);
            let label = if confirm { "Yes" } else { "No" };
            let row = lines
                .iter()
                .position(|line| line.contains(" No ") && line.contains(" Yes "))
                .expect("the confirm draws both buttons") as u16;
            let col = column_of(&lines[row as usize], label).unwrap();

            click(&mut app, col, row);
            assert!(
                !matches!(
                    app.overlay,
                    Some(crate::app::Overlay::ConfirmRemovePackage { .. })
                ),
                "clicking {label} answered the dialog instead of doing nothing"
            );
        }
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

    /// `click_flash_stack`'s own safety net --- `click_build_stack` had one
    /// already, this one did not.
    #[test]
    fn a_click_on_a_flash_button_selects_and_presses_it() {
        let root = project_dir("flashbtn", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.browser = Some(Browser::new(&root));
        app.show_device_actions_tab();
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|line| line.contains("Search firmware online"))
            .expect("the stack draws Search firmware online") as u16;
        let col = column_of(&lines[row as usize], "Search firmware online").unwrap();
        let before = app.logs.len();
        click(&mut app, col, row);
        assert_eq!(app.focus, Focus::FilesDevice);
        let flash = app.flash.as_ref().unwrap();
        assert_eq!(
            flash.pane_cursor, 0,
            "the clicked row (the stack's leading one) took the cursor"
        );
        assert!(
            app.logs.len() > before,
            "the click ran the button, not just its selection --- with no \
             chip connected, pressing it logs the same warning `Enter` would"
        );
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

    /// Inside the box but not on a button, a click answers nothing --- the
    /// dialog waits. (The name used to say "beside", which is now the
    /// opposite policy: a click *outside* the box dismisses it, which the
    /// `…_outside_the_box_closes_it…` test pins. Column 50 was always inside
    /// the 72-wide destructive box, so what this has tested all along is the
    /// message area.)
    #[test]
    fn a_click_on_a_confirms_message_area_answers_nothing() {
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
        // Column 50 sits inside the 72-wide box, on the quoted command.
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

    /// A point clearly outside any centered popup on a 100x40 test frame ---
    /// the corner, never inside a dialog's drawn rect.
    const OUTSIDE: (u16, u16) = (0, 0);

    /// The confirm family: a click outside the dialog closes it exactly
    /// like `Esc` --- as safe as declining the same question, never a
    /// silent no-op.
    #[test]
    fn a_click_outside_a_confirm_dialog_dismisses_it_like_esc() {
        let root = project_dir("outside-confirm", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.overlay = Some(crate::app::Overlay::ConfirmBuild {
            action: BuildAction::Flash,
            confirm: false,
        });
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside the dialog must close it, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `ConfirmRemovePackage` is missing from the confirm family's own
    /// button hit-testing (a pre-existing, separate gap) --- but it still
    /// needs a rect to know an outside click, and closing it must go
    /// through the same decline path Esc does (back to the package
    /// manager, never a flat dismiss).
    #[test]
    fn a_click_outside_confirm_remove_package_returns_to_packages_like_esc() {
        let root = project_dir("outside-remove-pkg", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::ConfirmRemovePackage {
            name: "umqtt.simple".to_string(),
            targets: vec![(
                crate::device::DevicePath::new("/lib/umqtt/simple.mpy"),
                false,
            )],
            declared: true,
            confirm: false,
        });
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            matches!(app.overlay, Some(crate::app::Overlay::Packages)),
            "declining this dialog always returns to the manager, Esc included"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A click clear of the popup box closes a plain list picker like Esc.
    #[test]
    fn a_click_outside_a_picker_dismisses_it_like_esc() {
        let root = project_dir("outside-picker", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::ThemePicker { selected: 0 });
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside the picker must close it, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The package manager's popup rect comes from the shared
    /// `layout::packages` helper (render and hit-testing already agree on
    /// it); an outside click closes it the same way Esc does.
    #[test]
    fn a_click_outside_the_packages_overlay_dismisses_it_like_esc() {
        let root = project_dir("outside-packages", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::Packages);
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside Packages must close it, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A docs picker (board/shield) uses the shared `layout::docs_picker`
    /// rect, spanning both its list and details panes --- a click past
    /// either one closes the whole modal.
    #[test]
    fn a_click_outside_a_docs_picker_dismisses_it_like_esc() {
        let root = project_dir("outside-docs", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.overlay = Some(crate::app::Overlay::BoardPicker {
            input: String::new(),
            selected: 0,
            scroll: 0,
            focus: DocsFocus::List,
        });
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside the docs picker must close it, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `list_row`/`button_at_row` key on the row alone, so it is not enough
    /// for the outside check to compare against the full frame's row band
    /// (an earlier version of this change did exactly that, and it broke
    /// here): a click that lands on the *right* row but past either edge of
    /// the popup must go through the overlay's own Esc answer instead of
    /// quietly answering whatever option that row names --- observed live
    /// in Zephyr Actions, SDK toolchains and the directory picker, so all
    /// three are pinned (SDK toolchains' own Esc steps back to the
    /// installer rather than closing outright, same as from the keyboard).
    #[test]
    fn a_click_on_the_right_row_but_outside_the_box_closes_it_instead_of_answering() {
        let root = project_dir("outside-row-za", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.overlay = Some(crate::app::Overlay::ZephyrActions { selected: 0 });
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("Update Zephyr"))
            .expect("the stack draws its leading button") as u16;
        click(&mut app, 0, row);
        assert!(
            app.overlay.is_none(),
            "same row, outside the box: closes rather than pressing Update Zephyr"
        );
        let _ = std::fs::remove_dir_all(&root);

        let root = project_dir("outside-row-sdk", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        // `draw_sdk_toolchains` draws nothing without a real installer.
        app.installer = Some(crate::install::Installer::new(root.clone()));
        app.overlay = Some(crate::app::Overlay::SdkToolchains { selected: 0 });
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("arm-zephyr-eabi"))
            .expect("the toolchain list is drawn") as u16;
        click(&mut app, 0, row);
        assert!(
            matches!(app.overlay, Some(crate::app::Overlay::ZephyrInstall)),
            "same row, outside the box: steps back to the installer (Esc's own answer here), \
             rather than toggling the toolchain"
        );
        let _ = std::fs::remove_dir_all(&root);

        let root = project_dir("outside-row-dir", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.overlay = Some(crate::app::Overlay::DirPicker {
            purpose: crate::workspace::DirPurpose::Installation,
            path: root.clone(),
            selected: 0,
            error: None,
        });
        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("use this directory"))
            .expect("the picker's leading row is drawn") as u16;
        click(&mut app, 0, row);
        assert!(
            app.overlay.is_none(),
            "same row, outside the box: closes rather than picking the directory"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The SDK toolchains modal draws in its declared `overlay_popup` rect
    /// --- a centered 56-wide box, not the whole frame --- and a click
    /// toggles the row under the pointer. The drawer once received `area`
    /// (the full frame) instead of `popup`: the modal went fullscreen while
    /// the hit-testing kept judging against the centered rect, so clicks
    /// selected rows above the pointer and a click on the modal's outer
    /// margin closed it.
    #[test]
    fn sdk_toolchains_draws_in_its_popup_and_clicks_land_on_the_pointer() {
        let root = project_dir("sdk-click", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.installer = Some(crate::install::Installer::new(root.clone()));
        app.overlay = Some(crate::app::Overlay::SdkToolchains { selected: 0 });

        let lines = render(&mut app, 120, 40);
        let title = lines
            .iter()
            .find(|l| l.contains("SDK toolchains"))
            .expect("the modal is drawn");
        // The modal's own span on its title row --- in drawn cells, not
        // bytes (the borders are multi-width); the dashboard stays visible
        // around the centered popup.
        let cells: Vec<char> = title.chars().collect();
        let start = cells.iter().position(|c| *c == '╭').unwrap();
        let end = cells.iter().position(|c| *c == '╮').unwrap();
        assert_eq!(
            end - start + 1,
            56,
            "the modal is the declared 56-wide box, not the frame:\n{}",
            lines.join("\n")
        );
        // Two rows of frame sit above the centered popup (frame minus 4,
        // centered): the title bar and one more line of the dashboard.
        assert_eq!(
            lines
                .iter()
                .position(|l| l.contains("SDK toolchains"))
                .unwrap(),
            2,
            "the popup centers vertically"
        );

        // A click on a toolchain's row toggles that toolchain. The column
        // is the name's own drawn cell (the popup starts at column 32 in a
        // 120-wide frame; column 3 would be an outside click).
        let aim = |lines: &[String], needle: &str| -> u16 {
            let line = lines.iter().find(|l| l.contains(needle)).unwrap();
            // Byte offsets are not columns --- the borders are multi-byte.
            line.find(needle)
                .map(|byte| line[..byte].chars().count() as u16)
                .unwrap_or(0)
        };
        let row = lines
            .iter()
            .position(|l| l.contains("riscv64-zephyr-elf"))
            .expect("the toolchain is drawn") as u16;
        click(&mut app, aim(&lines, "riscv64-zephyr-elf"), row);
        assert!(
            app.installer
                .as_ref()
                .unwrap()
                .picked_toolchains
                .iter()
                .any(|name| name == "riscv64-zephyr-elf"),
            "the clicked row is the row that toggled:\n{}",
            lines.join("\n")
        );

        // A shorter frame scrolls the 37-row list; the pointer still lands
        // on the drawn row, through the same fresh-state offset the overlay
        // draws with.
        app.installer.as_mut().unwrap().picked_toolchains.clear();
        let lines = render(&mut app, 100, 32);
        let row = lines
            .iter()
            .position(|l| l.contains("riscv64-zephyr-elf"))
            .expect("the toolchain is drawn") as u16;
        click(&mut app, aim(&lines, "riscv64-zephyr-elf"), row);
        assert!(
            app.installer
                .as_ref()
                .unwrap()
                .picked_toolchains
                .iter()
                .any(|name| name == "riscv64-zephyr-elf"),
            "the scrolled list answers the pointer the same way:\n{}",
            lines.join("\n")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A click clear of the whole popup (the corner) closes Zephyr Actions
    /// like Esc; `a_click_on_the_right_row_but_outside_the_box_closes_it_instead_of_answering`
    /// above covers the narrower same-row case.
    #[test]
    fn a_click_outside_zephyr_actions_dismisses_it_like_esc() {
        let root = project_dir("outside-zactions", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.overlay = Some(crate::app::Overlay::ZephyrActions { selected: 0 });
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside Zephyr Actions must close it, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The installer, with no installation running: Esc's own guard
    /// (`!installer.is_busy()`) applies unchanged, so an outside click
    /// closes it exactly as it would with no installer at all.
    #[test]
    fn a_click_outside_the_zephyr_installer_dismisses_it_like_esc() {
        let root = project_dir("outside-install", 1);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.overlay = Some(crate::app::Overlay::ZephyrInstall);
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside the installer must close it, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The help window's popup depends on the filter text and the current
    /// view --- the one formula worth porting carefully. Outside the
    /// filter, an outside click closes the window flat; while filtering,
    /// it must step back to the cursor first (the second press closes it),
    /// exactly like Esc from the keyboard.
    #[test]
    fn a_click_outside_help_dismisses_it_like_esc() {
        let root = project_dir("outside-help", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::Help {
            filter: String::new(),
            filtering: false,
            selected: 0,
        });
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside Help must close it flat, like Esc when not filtering"
        );

        app.overlay = Some(crate::app::Overlay::Help {
            filter: "flash".to_string(),
            filtering: true,
            selected: 0,
        });
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            matches!(
                app.overlay,
                Some(crate::app::Overlay::Help {
                    filtering: false,
                    ..
                })
            ),
            "while filtering, an outside click steps back first, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The file viewer has no click surface of its own --- typing is its
    /// only gesture --- but its popup rect is a plain function of the
    /// frame size, so an outside click closes it like every other overlay.
    #[test]
    fn a_click_outside_the_file_viewer_dismisses_it_like_esc() {
        let root = project_dir("outside-viewer", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::FileViewer);
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside the viewer must close it, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A text-input dialog (`CreateEntry`/`RenameEntry`) is deliberately not
    /// clickable inside --- but an outside click still cancels it, the same
    /// way Esc does.
    #[test]
    fn a_click_outside_create_entry_dismisses_it_like_esc() {
        let root = project_dir("outside-create", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.overlay = Some(crate::app::Overlay::CreateEntry {
            side: Side::Local,
            input: String::new(),
        });
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert!(
            app.overlay.is_none(),
            "a click outside the create-entry dialog must close it, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The Flash dialog (`View::Flash`) is a structurally separate modal
    /// from `Overlay`, with its own outside-click check. On the top-level
    /// menu, `Esc` leaves the dialog entirely.
    #[test]
    fn a_click_outside_the_flash_menu_dismisses_it_like_esc() {
        let root = project_dir("outside-flash-menu", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.show_device_actions_tab();
        app.view = View::Flash;
        assert_eq!(app.flash.as_ref().unwrap().screen, FlashScreen::Menu);
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert_eq!(
            app.view,
            View::Dashboard,
            "a click outside the flash menu must leave the dialog, like Esc"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// On a dialog *screen* below the menu (Options, the online searches,
    /// the URL entry), `Esc` steps back one level instead of leaving
    /// outright (`leave_flash_screen`) --- an outside click must reuse
    /// exactly that, not a flat close.
    #[test]
    fn a_click_outside_a_flash_screen_steps_back_like_esc() {
        let root = project_dir("outside-flash-options", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.show_device_actions_tab();
        app.flash.as_mut().unwrap().screen = FlashScreen::Options;
        app.view = View::Flash;
        render(&mut app, 100, 40);
        click(&mut app, OUTSIDE.0, OUTSIDE.1);
        assert_eq!(
            app.flash.as_ref().unwrap().screen,
            FlashScreen::Menu,
            "an outside click steps back one screen, like Esc"
        );
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
        let col = column_of(&lines[row as usize], "Update Zephyr").unwrap();
        click(&mut app, col, row);
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
            ("the build report, read here", 2),
            ("Dashboard (HTML)", 3),
            ("west build -t dashboard", 3),
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
                // The TUI dashboard replaces the menu with its own window
                // rather than starting anything.
                2 => assert!(
                    matches!(overlay, Some(crate::app::Overlay::BuildDashboard)),
                    "{label}: the dashboard opens its window"
                ),
                _ => assert!(
                    app.build.as_ref().is_some_and(|panel| panel.is_busy())
                        || app.log_tab == LogTab::Monitor,
                    "{label}: the HTML report starts through the panel"
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
        let col = column_of(&lines[row as usize], "Tokyo Night").unwrap();
        click(&mut app, col, row);
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

        // A click that lands on neither pane (the preview thumbnail, below
        // the list) must not steal the keyboard back from Details.
        let frame = app.frame_area.unwrap();
        let preview = crate::ui::layout::docs_picker(frame).preview;
        click(&mut app, preview.x + 1, preview.y + 1);
        assert!(
            matches!(
                app.overlay,
                Some(Overlay::BoardPicker {
                    focus: DocsFocus::Details,
                    ..
                })
            ),
            "a click on the preview pane must not reset focus to the list"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A wheel step inside a docs picker scrolls the pane under the
    /// pointer, pinned against the drawn frame: over the west list it
    /// walks the rows one per event (clamped at the ends, never wrapping)
    /// and restarts the details from its top like every cursor move; over
    /// the details it scrolls the text one line at a time without handing
    /// it the keyboard.
    #[test]
    fn a_docs_picker_wheel_scrolls_the_pane_under_the_pointer() {
        let root = project_dir("docswheel", 0);
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

        // Publish the frame's geometry, then aim the steps at the panes
        // the frame drew (the same shared `docs_picker` tree the click
        // hit-tests through).
        render(&mut app, 100, 40);
        let frame = app.frame_area.unwrap();
        let areas = crate::ui::layout::docs_picker(frame);

        // Up at the top of the list clamps: the selection stays put (the
        // arrows wrap; a wheel that wraps feels like a bug).
        wheel(&mut app, -1, areas.list.x + 2, areas.list.y + 2);
        assert!(
            matches!(app.overlay, Some(Overlay::BoardPicker { selected: 0, .. })),
            "a wheel step at the top of the list clamps"
        );

        // Down walks one row per event, not a page.
        wheel(&mut app, 1, areas.list.x + 2, areas.list.y + 2);
        if let Some(Overlay::BoardPicker { selected, .. }) = &app.overlay {
            assert_eq!(*selected, 1, "one wheel step down walks one row");
        }

        // A step over the details scrolls its text and never moves the
        // keyboard --- the row cursor stays where it was.
        wheel(&mut app, 1, areas.details.x + 2, areas.details.y + 2);
        let picker = std::matches!(&app.overlay, Some(Overlay::BoardPicker { .. }));
        assert!(picker, "the picker is still open");
        if let Some(Overlay::BoardPicker {
            selected,
            scroll,
            focus,
            ..
        }) = &app.overlay
        {
            assert_eq!(*selected, 1, "the row cursor did not move");
            assert_eq!(*scroll, 1, "the details scrolled a line");
            assert_eq!(*focus, DocsFocus::List, "the keyboard did not move");
        }

        // The next list step resets the details to the top --- the new
        // row's text starts where every cursor move starts it.
        wheel(&mut app, 1, areas.list.x + 2, areas.list.y + 2);
        if let Some(Overlay::BoardPicker {
            selected, scroll, ..
        }) = &app.overlay
        {
            assert_eq!(*selected, 2, "the walk continued");
            assert_eq!(*scroll, 0, "a moved row restarts the details from its top");
        }

        // The preview pane below the list scrolls nothing.
        wheel(&mut app, 1, areas.preview.x + 1, areas.preview.y + 1);
        if let Some(Overlay::BoardPicker {
            selected, scroll, ..
        }) = &app.overlay
        {
            assert_eq!(*selected, 2, "the row cursor did not move");
            assert_eq!(*scroll, 0, "a wheel step over the preview pane is ignored");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The shield picker answers the same wheel grammar over its own list:
    /// row 0 is the `(none)` row, so the count --- and the clamp at the
    /// bottom --- includes it.
    #[test]
    fn a_shield_picker_wheel_counts_the_none_row() {
        let root = project_dir("shieldwheel", 0);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.build.as_mut().unwrap().set_tool_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/bin/west"
        ));
        app.open_shield_picker();

        // Drain the background `west shields` fetch to its list.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            for event in app.processes.drain() {
                app.handle(AppEvent::Process(event));
            }
            if matches!(
                app.build.as_ref().unwrap().shields.state,
                crate::build::ListState::Loaded(_)
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the fake west shields never finished"
            );
        }

        render(&mut app, 100, 40);
        let frame = app.frame_area.unwrap();
        let areas = crate::ui::layout::docs_picker(frame);

        // The fixture carries three shields, so the list is four rows
        // (`(none)` included): steps walk it a row at a time and clamp at
        // the last instead of wrapping back to `(none)`.
        wheel(&mut app, 1, areas.list.x + 2, areas.list.y + 2);
        if let Some(Overlay::ShieldPicker { selected, .. }) = &app.overlay {
            assert_eq!(*selected, 1, "one step walks one row");
        }
        for _ in 0..5 {
            wheel(&mut app, 1, areas.list.x + 2, areas.list.y + 2);
        }
        if let Some(Overlay::ShieldPicker { selected, .. }) = &app.overlay {
            assert_eq!(*selected, 3, "steps past the end clamp on the last row");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A click on a visible row keeps the list exactly where it was. The
    /// west list used to render with a fresh `ListState` every frame, so
    /// its offset was recomputed from zero --- anchoring any selection in
    /// the lower half of a long list at the pane's bottom edge --- and a
    /// click (mapped through that assumed offset) jumped the view downward
    /// to re-anchor the row it selected. The offset now persists across
    /// frames (`App::docs_list_offset`), pinned here against a long list:
    /// the clicked row is the row the pointer rested on, and the top
    /// visible row does not move.
    #[test]
    fn a_docs_picker_click_keeps_the_list_where_it_was() {
        let root = project_dir("docsstay", 0);
        let mut app = app_with_backend(BackendKind::Zephyr, &root);
        app.build.as_mut().unwrap().set_tool_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/bin/west"
        ));
        app.open_board_picker();

        // Drain the background `west boards` fetch, then replace the list
        // with one long enough that the pane cannot hold it.
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
        let boards: Vec<crate::build::Board> = (0..60)
            .map(|i| crate::build::Board {
                name: format!("board_{i:02}"),
                description: format!("test board {i}"),
            })
            .collect();
        app.build.as_mut().unwrap().boards.state = crate::build::ListState::Loaded(boards);

        // Wheel well past the pane's bottom edge, then render so the
        // settled offset is published and the rows are drawn.
        render(&mut app, 100, 40);
        let frame = app.frame_area.unwrap();
        let areas = crate::ui::layout::docs_picker(frame);
        let inner_top = areas.list.y + 1;
        for _ in 0..20 {
            wheel(&mut app, 1, areas.list.x + 2, inner_top);
        }
        let lines = render(&mut app, 100, 40);
        let offset = app.docs_list_offset;
        assert!(offset > 0, "the wheel scrolled the list (offset {offset})");
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!("board_{offset:02}"))),
            "the row at the settled offset leads the pane"
        );

        // Click a row in the middle of the pane: it must select exactly
        // the row under the pointer, and the view must not shift.
        let clicked = offset + 4;
        click(&mut app, areas.list.x + 2, inner_top + 4);
        if let Some(Overlay::BoardPicker { selected, .. }) = &app.overlay {
            assert_eq!(*selected, clicked, "the click selected the drawn row");
        }
        let lines = render(&mut app, 100, 40);
        assert_eq!(
            app.docs_list_offset, offset,
            "clicking a visible row does not move the list"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!("board_{offset:02}"))),
            "the same row still leads the pane after the click"
        );

        // And the keyboard keeps working over the same geometry: the view
        // follows the cursor only once it leaves the window, and then
        // slides one row at a time --- the persistent offset is the
        // arrows' anchor too, not just the mouse's.
        let mut presses = 0;
        while app.docs_list_offset == offset && presses < 40 {
            app.on_key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            ));
            render(&mut app, 100, 40);
            presses += 1;
        }
        assert!(
            presses < 40,
            "the cursor eventually reaches the window's edge"
        );
        assert_eq!(
            app.docs_list_offset,
            offset + 1,
            "the view slides exactly one row past the bottom edge"
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
        assert_eq!(
            app.focus,
            Focus::DeviceInfo,
            "the click also focuses the pane"
        );

        // A neighbouring row (the firmware identity) is not a copy target,
        // but it still focuses the pane like every other row would.
        app.focus = Focus::Project;
        render(&mut app, 100, 40);
        let other = lines.iter().position(|l| l.contains("Firmware:")).unwrap() as u16;
        click(&mut app, mac_col, other);
        assert!(
            app.take_clipboard_request().is_none(),
            "only the MAC row copies"
        );
        assert_eq!(app.focus, Focus::DeviceInfo);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Clicking the Device Info pane focuses it even with no MAC known yet
    /// --- focus is not conditioned on the pane having anything to copy.
    #[test]
    fn a_click_on_the_device_info_pane_focuses_it_without_a_mac() {
        let root = project_dir("mac-none", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.show_device_actions_tab();
        assert!(app.flash.as_ref().unwrap().details.mac.is_none());

        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("no device data yet"))
            .expect("the empty-state placeholder is drawn") as u16;
        let col = column_of(&lines[row as usize], "no device data").unwrap();
        app.focus = Focus::Project;
        click(&mut app, col, row);
        assert_eq!(app.focus, Focus::DeviceInfo);
        assert!(app.take_clipboard_request().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Before anything is identified, a double click on the Device Info
    /// pane is `ctrl+r`'s mouse twin: it offers the same identification
    /// question, not a copy (there is nothing to copy yet).
    #[test]
    fn a_double_click_on_the_device_info_pane_offers_identification() {
        let root = project_dir("mac-double", 1);
        let mut app = app_with_backend(BackendKind::MicroPython, &root);
        app.show_device_actions_tab();
        app.devices.set_devices(vec![crate::device::DeviceInfo {
            port: "/dev/ttyACM0".to_string(),
            serial: None,
            vid_pid: String::new(),
            description: String::new(),
        }]);
        app.devices.select(0);

        let lines = render(&mut app, 100, 40);
        let row = lines
            .iter()
            .position(|l| l.contains("no device data yet"))
            .expect("the empty-state placeholder is drawn") as u16;
        let col = column_of(&lines[row as usize], "no device data").unwrap();

        click(&mut app, col, row);
        assert!(
            app.overlay.is_none(),
            "a single click must not offer it yet"
        );
        click(&mut app, col, row);
        assert!(
            matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
            "the double click must offer the identification, like ctrl+r"
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
