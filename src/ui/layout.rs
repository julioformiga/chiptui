//! The dashboard's geometry as a pure function of [`App`] state.
//!
//! One definition of where every pane sits, consumed by both the renderer
//! ([`crate::ui::draw`]) and the mouse hit-testing (`crate::app::mouse`):
//! a click must land on exactly the rect the frame drew, so the two cannot
//! each carry their own copy of the splits. Recomputing the tree is a
//! handful of `Layout` solves --- cheap enough to run per frame and per
//! gesture, and free of cached state that a resize would stale.

use ratatui::layout::{Constraint, Layout, Rect};

use crate::app::{App, FileAction, Overlay, ThemeChoice};
use crate::backend::{BackendKind, Capability};
use crate::browser::SyncPlan;
use crate::flash::FlashAction;

/// The minimum rows the workspace pane's embedded file list gets, past the
/// checklist and its header line --- enough to show a handful of entries
/// without scrolling. Row 2 is sized to fit exactly this much (like the
/// checklist rows above it); a taller terminal gives the remainder to row 3
/// (the log/monitor pane), the same trade-off row 2 already makes today.
const MIN_FILES_ROWS: u16 = 6;

/// What claims the right half of a browser row --- capability-gated, like
/// every row-2 decision (`AGENTS.md` §3), so the enum says what draws there
/// without naming a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RightKind {
    /// The device pane: the backend browses a device filesystem
    /// (`Capability::Filesystem`), with or without its actions strip.
    Device,
    /// The build panel: a browser row for a backend without `Filesystem`
    /// that can still build.
    Build,
    /// The honest "no device filesystem" placeholder.
    NoDevice,
}

/// Row 2's panes, whichever shape the backend's capabilities give the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row2 {
    /// The workspace + build pair: a backend that maintains a shared
    /// environment and builds without a device filesystem.
    WorkspaceBuild { workspace: Rect, build: Rect },
    /// The dual-pane file browser: local files on the left, the right half
    /// claimed by [`RightKind`], the optional legend strip below.
    Browser(BrowserRow),
    /// The window before any pane exists: the no-filesystem placeholder
    /// owns the whole row.
    Placeholder(Rect),
}

/// The browser row's panes, split out of [`Row2::Browser`] so the renderer
/// and the hit-tester can destructure once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrowserRow {
    pub(crate) local: Rect,
    pub(crate) right: Rect,
    pub(crate) right_kind: RightKind,
    /// The comparison-marker legend's strip, present only when the device
    /// pane is showing files (the actions tab claims the row's full height
    /// instead, so the legend yields its line while that tab is showing).
    pub(crate) legend: Option<Rect>,
}

/// Every pane rect of the dashboard body --- the answers a click and a
/// frame both ask for. Row 1's panes are fixed-height informational; row 3
/// is one pane whose top border carries the Log/Monitor/Terminal strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DashboardAreas {
    pub(crate) project: Rect,
    pub(crate) device: Rect,
    pub(crate) row2: Row2,
    pub(crate) row3: Rect,
}

/// Where the dashboard's panes sit inside `body`, the same tree
/// `draw_dashboard` walked inline before geometry grew a second consumer:
/// row 1 (fixed `panels::INFO_ROWS` + borders) over rows 2/3; row 2 sized
/// to its stacked content when a pane claims it, else the historical 60/40;
/// each row split into halves by percentage.
pub(crate) fn dashboard(app: &App, body: Rect) -> DashboardAreas {
    // `ctrl+f`: row 3 claims the whole body, panes 1/2 degenerate to
    // zero-size rects --- the one shared definition mouse hit-testing and
    // the renderer both read, so a click cannot land on a pane that was
    // never drawn and `draw_dashboard` can skip drawing it outright.
    if app.row3_fullscreen {
        return DashboardAreas {
            project: Rect::default(),
            device: Rect::default(),
            row2: Row2::Placeholder(Rect::default()),
            row3: body,
        };
    }

    let info_height = super::panels::INFO_ROWS as u16 + 2;

    let [row1, rest] =
        Layout::vertical([Constraint::Length(info_height), Constraint::Min(0)]).areas(body);
    // Row 2 leans on its content when the workspace/project panes or the
    // device pane's strip claim it: their stacked button groups (checklist
    // rows, a separator, a rule per button edge, the pinned state line)
    // are the tallest content on the dashboard, so the row is sized to
    // fit them and the log pane (which scrolls) takes the remainder. The
    // device pane sizes the row whenever its *strip* exists, not only
    // while the actions tab is showing: switching the two tabs must not
    // reflow the rows below, so the files tab rides at the actions tab's
    // height instead of the browser's historical 60/40 split.
    let [row2, row3] = if app.workspace_pane_visible() || app.device_actions_tab_available() {
        let needed = row2_content_height(app)
            .saturating_add(2) // the pane's borders (the state line is content, already counted)
            .min(rest.height.saturating_sub(3).max(1));
        Layout::vertical([Constraint::Length(needed), Constraint::Min(0)]).areas(rest)
    } else {
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(rest)
    };
    let [project, device] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(row1);

    let row2_areas = if app.workspace_pane_visible() {
        let [workspace, build] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(row2);
        Row2::WorkspaceBuild { workspace, build }
    } else if app.browser.is_some() {
        // The legend explains the comparison markers, which only exist when
        // there is a device pane to compare against; without it the row's
        // last line is dead weight for the local pane. The actions tab
        // claims the row's full height instead (its stack is the tallest
        // content the row shows), so the legend yields its line while that
        // tab is showing.
        let has_filesystem = app.manager.capabilities().contains(Capability::Filesystem);
        let (panes, legend) = if has_filesystem && !app.device_actions_tab_active() {
            let [panes, legend] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(row2);
            (panes, Some(legend))
        } else {
            (row2, None)
        };
        let [local, right] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(panes);
        let right_kind = if has_filesystem {
            RightKind::Device
        } else if app.build_pane_visible() {
            RightKind::Build
        } else {
            RightKind::NoDevice
        };
        Row2::Browser(BrowserRow {
            local,
            right,
            right_kind,
            legend,
        })
    } else {
        Row2::Placeholder(row2)
    };

    DashboardAreas {
        project,
        device,
        row2: row2_areas,
        row3,
    }
}

/// The taller row-2 pane's inner content height: the project-files pane's
/// minimum listing rows on one side (the walked path lives on its border
/// now, so the listing is the whole content); the project pane's stacked
/// button group --- one row per button, one rule at each edge and one
/// divider between each pair --- on the other.
fn row2_content_height(app: &App) -> u16 {
    let caps = app.manager.capabilities();
    let workspace = app.workspace.as_ref().map_or(0, |_| MIN_FILES_ROWS);
    let build = app.build.as_ref().map_or(0, |panel| {
        // The stacked group plus a three-row footer, reserved whether or
        // not the `Stop` box is showing (`Stop` is appended to the list,
        // never a stacked row, so the group itself never changes size):
        // the pane's height must not change when a command starts.
        let mains = panel.actions(&caps).len() - usize::from(panel.is_busy());
        (2 * mains + 1 + 3) as u16
    });
    // The device pane's strip sizes the row by the same rule whenever it
    // exists: its stack is the tallest content the browser row has, a
    // clipped button is one the user cannot press, and the files tab must
    // not sit at a different height than the actions tab beside it. With
    // no panel yet (nothing background-created one), the stack the first
    // entry onto the tab will draw is `FlashAction::ALL` --- ChipInfo is
    // filtered out and SearchOnline added back, so the idle count equals
    // it, and `Stop` is pinned in the reserved footer rather than the
    // stack, so busy does not change the number either.
    let actions = if app.device_actions_tab_available() {
        // The no-panel fallback goes through the same stack formula: the
        // row is sized for the stack the first entry onto the tab will
        // draw, so creating the panel must not reflow the rows below.
        let mains = app.flash.as_ref().map_or(FlashAction::ALL.len(), |flash| {
            flash.pane_actions().len() - usize::from(flash.is_busy())
        });
        (2 * mains + 1 + 3) as u16
    } else {
        0
    };
    workspace.max(build).max(actions)
}

/// The board/shield pickers' pane rects: the modal fills the frame minus
/// one column per side and two rows above and below (a frame at the
/// declared minimum still fits whole panes), its body split into the west
/// list with the preview below it and the details column. One definition
/// shared by the renderer ([`crate::ui::overlay`]) and the click
/// hit-testing (`crate::app::mouse`), like [`dashboard`] below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DocsPickerAreas {
    /// The modal itself (border included) --- the `Clear` rect.
    pub(crate) popup: Rect,
    /// The search/filter line, above the hint.
    pub(crate) filter: Rect,
    /// The wrapped hint line(s) between the filter and the body.
    pub(crate) hint: Rect,
    /// The west list pane (border included).
    pub(crate) list: Rect,
    /// The preview pane under it (border included).
    pub(crate) preview: Rect,
    /// The details column (border included).
    pub(crate) details: Rect,
}

/// Where the docs pickers' panes sit inside `area`: the modal takes the
/// whole frame minus one column per side and two rows above and below;
/// under the filter and hint lines the body fixes the west list's column
/// at 32 (the list above, the preview below it), so every column a wider
/// terminal adds goes to the details pane --- the list's rows are the
/// picker's spine and fit their fixed column, while the docs text is the
/// part more width genuinely buys. The preview takes a bounded minority
/// of the column --- never more than the column minus the rows the list
/// needs to stay usable.
pub(crate) fn docs_picker(area: Rect) -> DocsPickerAreas {
    let popup = super::centered(
        area,
        area.width.saturating_sub(2),
        area.height.saturating_sub(4),
    );
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let [filter, hint, body] = Layout::vertical([
        Constraint::Length(1), // the search/filter line
        Constraint::Length(2), // the wrapped hint
        Constraint::Min(1),
    ])
    .areas(inner);
    let [left, details] =
        Layout::horizontal([Constraint::Length(32), Constraint::Min(1)]).areas(body);
    let preview = ((left.height as u32 * 2 / 5) as u16)
        .clamp(4, 14)
        .min(left.height.saturating_sub(4));
    let [list, preview] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(preview)]).areas(left);
    DocsPickerAreas {
        popup,
        filter,
        hint,
        list,
        preview,
        details,
    }
}

/// Where the package manager's panes sit inside `area`.
///
/// The docs pickers' modal geometry ([`docs_picker`]) without the preview
/// split: a package has no picture to fetch, so the whole left column is
/// the list. The left column is wider than the pickers' 32 --- a row
/// carries a mark, a name and a version --- and every column a wider
/// terminal adds still goes to the details pane.
pub(crate) struct PackagesAreas {
    /// The modal itself (border included) --- the `Clear` rect.
    pub(crate) popup: Rect,
    /// The filter line, which doubles as the manual-spec field.
    pub(crate) filter: Rect,
    /// The wrapped hint line(s) between the filter and the body.
    pub(crate) hint: Rect,
    /// The row list (border included).
    pub(crate) list: Rect,
    /// The details column (border included).
    pub(crate) details: Rect,
}

/// Columns the row list keeps whatever the terminal's width: a mark, a
/// name and a version fit; the description lives in the details pane.
pub(crate) const PACKAGES_LIST_WIDTH: u16 = 44;

pub(crate) fn packages(area: Rect) -> PackagesAreas {
    let popup = super::centered(
        area,
        area.width.saturating_sub(2),
        area.height.saturating_sub(4),
    );
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let [filter, hint, body] = Layout::vertical([
        Constraint::Length(1), // the filter/spec line
        Constraint::Length(2), // the wrapped hint
        Constraint::Min(1),
    ])
    .areas(inner);
    let [list, details] = Layout::horizontal([
        Constraint::Length(PACKAGES_LIST_WIDTH.min(body.width)),
        Constraint::Min(1),
    ])
    .areas(body);
    PackagesAreas {
        popup,
        filter,
        hint,
        list,
        details,
    }
}

/// Where the build dashboard's panes sit inside `area`.
///
/// [`packages`]' geometry with a tab strip added --- and the strip costs no
/// content row, because it rides the modal's *top border* the way row 3's
/// Log/Monitor/Terminal strip rides its pane's. At the declared 80x32
/// minimum the body is 23 rows, and spending one of them on a strip that
/// has a border to sit on would be paying twice.
pub(crate) struct BuildDashboardAreas {
    /// The modal itself (border included) --- the `Clear` rect.
    pub(crate) popup: Rect,
    /// The tab strip, on the modal's top border row.
    pub(crate) strip: Rect,
    /// The filter line.
    pub(crate) filter: Rect,
    /// The wrapped hint line(s) between the filter and the body.
    pub(crate) hint: Rect,
    /// The row list (border included).
    pub(crate) list: Rect,
    /// The details column (border included).
    pub(crate) details: Rect,
}

/// Columns the row list keeps whatever the terminal's width.
///
/// Wider than the docs pickers' 32 and narrower than
/// [`PACKAGES_LIST_WIDTH`]: `CONFIG_DT_HAS_CHIPSEMI_CHSC6X_ENABLED` is 37
/// characters, and a Kconfig row that cannot show its own symbol name is
/// not a row. Every column a wider terminal adds still goes to the details.
pub(crate) const BUILD_DASHBOARD_LIST_WIDTH: u16 = 42;

pub(crate) fn build_dashboard(area: Rect) -> BuildDashboardAreas {
    let popup = super::centered(
        area,
        area.width.saturating_sub(2),
        area.height.saturating_sub(4),
    );
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let strip = Rect {
        x: popup.x.saturating_add(1),
        y: popup.y,
        width: popup.width.saturating_sub(2),
        height: 1,
    };
    let [filter, hint, body] = Layout::vertical([
        Constraint::Length(1), // the filter line
        Constraint::Length(2), // the wrapped hint
        Constraint::Min(1),
    ])
    .areas(inner);
    let [list, details] = Layout::horizontal([
        Constraint::Length(BUILD_DASHBOARD_LIST_WIDTH.min(body.width)),
        Constraint::Min(1),
    ])
    .areas(body);
    BuildDashboardAreas {
        popup,
        strip,
        filter,
        hint,
        list,
        details,
    }
}

/// The width every destructive/confirm dialog is drawn at.
pub(crate) const DESTRUCTIVE_WIDTH: u16 = 72;

/// The rect an open overlay draws its popup in.
///
/// The single definition, consumed by `ui::overlay`'s drawing *and* by
/// `app::mouse`'s hit-testing --- the same contract [`packages`] and
/// [`docs_picker`] already had, extended to every other variant. Both sides
/// used to carry the numbers separately, and the pairs drifted: the device
/// and firmware pickers draw a fixed 52x4 when their list is empty, which
/// the hit-testing never mirrored, so a click *inside* the empty device
/// picker was judged outside and dismissed it.
///
/// A variant whose shape depends on state (the confirm family's three
/// `Confirm` sizes, the sync preview's line count, the help table's widest
/// binding) reads it off `app` here rather than in two places.
pub(crate) fn overlay_popup(app: &App, overlay: &Overlay, frame: Rect) -> Rect {
    let (width, height) = match overlay {
        // ---- the confirm family: fixed boxes, one per wording ----------
        Overlay::Confirm { .. } if app.install_confirm_pending => (DESTRUCTIVE_WIDTH, 10),
        Overlay::Confirm { .. }
            if app
                .flash
                .as_ref()
                .is_some_and(|flash| flash.pending().is_some()) =>
        {
            (DESTRUCTIVE_WIDTH, 9)
        }
        Overlay::Confirm { .. } => (70, 7),
        Overlay::ConfirmBuild { .. }
        | Overlay::ConfirmInstallHere { .. }
        | Overlay::ConfirmRemovePackage { .. } => (DESTRUCTIVE_WIDTH, 9),
        Overlay::ConfirmRestartDevice { .. } => (54, 8),
        Overlay::ConfirmSwitchProject { .. } => (62, 9),
        Overlay::ConfirmEraseForMicroPython { .. } => (65, 9),
        Overlay::ConfirmIdentifyDevice { .. } => (66, 11),
        Overlay::ConfirmInterruptDevice { .. } => (64, 10),
        Overlay::ConfirmDelete { .. } => (54, 9),
        Overlay::ConfirmDownloadOverwrite { .. } => (70, 8),
        Overlay::ConfirmUpload { .. } => (65, 8),
        Overlay::SyncPreview { plan, .. } if plan.is_empty() => (58, 7),
        Overlay::SyncPreview { plan, .. } => (
            60,
            (sync_preview_lines(plan) as u16 + 5).min(frame.height.saturating_sub(2)),
        ),

        // ---- lists: one row per entry, inside the modal's two rules ----
        //
        // An empty list is not `len + 2`: both pickers draw a fixed box
        // carrying the "nothing found" message instead, and the rect has
        // to follow or a click on that message reads as a miss.
        Overlay::DevicePicker { .. } => match app.devices.devices().len() {
            0 => (52, 4),
            len => (64, len as u16 + 2),
        },
        Overlay::FirmwarePicker { .. } => {
            let len = app
                .flash
                .as_ref()
                .map(|flash| flash.firmware.len())
                .unwrap_or(0);
            match len {
                0 => (52, 4),
                len => (64, len as u16 + 2),
            }
        }
        Overlay::ThemePicker { .. } => (44, ThemeChoice::all().len() as u16 + 2),
        Overlay::ProjectSetup { .. } => (60, BackendKind::ALL.len() as u16 + 4),
        Overlay::RestoreDeviceScript { .. } => (64, RESTORE_CHOICES as u16 + 4),
        Overlay::FileActions {
            side,
            name,
            is_dir,
            status,
            ..
        } => {
            let actions = FileAction::for_entry(
                *side,
                *is_dir,
                crate::files::is_text_like(name),
                *status,
                app.manager.capabilities(),
            );
            (44, actions.len() as u16 + 2)
        }
        Overlay::ZephyrActions { .. } => {
            let placeholders: Vec<super::Button> = (0..super::ZEPHYR_ACTIONS_COUNT)
                .map(|_| super::Button::new("").detail(""))
                .collect();
            (64, super::stack_height(&placeholders).saturating_add(2))
        }

        // ---- fixed-shape modals ----------------------------------------
        Overlay::DirPicker { .. } | Overlay::ProjectPicker { .. } => (72, 18),
        Overlay::BuildDirPicker { .. } => (60, 16),
        Overlay::SdkToolchains { .. } => (56, frame.height.saturating_sub(4)),
        Overlay::CreateEntry { .. } | Overlay::RenameEntry { .. } => (54, 6),
        Overlay::FileViewer => (
            frame.width.saturating_sub(6).max(20),
            frame.height.saturating_sub(4).max(6),
        ),
        Overlay::Help { filter, .. } => help_size(app, filter, frame),

        // ---- variants that already own a shared geometry helper --------
        Overlay::Packages => return packages(frame).popup,
        Overlay::BuildDashboard => return build_dashboard(frame).popup,
        Overlay::BoardPicker { .. } | Overlay::ShieldPicker { .. } => {
            return docs_picker(frame).popup;
        }
        Overlay::ZephyrInstall => return super::install_area(frame),
    };
    super::centered(frame, width, height)
}

/// [`Overlay::RestoreDeviceScript`]'s three choices --- reset, restart
/// `main.py`, leave stopped (`ui::overlay`'s own `CHOICES`).
const RESTORE_CHOICES: usize = 3;

/// [`Overlay::Help`]'s `(width, height)`: the table is as wide as its widest
/// binding and as tall as the bindings the filter left standing, both
/// clamped to the frame.
fn help_size(app: &App, filter: &str, frame: Rect) -> (u16, u16) {
    use crate::app::help::{self, HelpSection};

    let navigation = help::visible(help::bindings(app.view, HelpSection::Navigation), filter);
    let commands = help::visible(help::bindings(app.view, HelpSection::Commands), filter);
    let key_col = HelpSection::ALL
        .iter()
        .flat_map(|&section| help::bindings(app.view, section))
        .map(|binding| binding.key.chars().count())
        .max()
        .unwrap_or(0);
    let indent = 2 + key_col + 2;
    let widest = HelpSection::ALL
        .iter()
        .flat_map(|&section| help::bindings(app.view, section))
        .map(|binding| indent + binding.description.chars().count() + 2)
        .max()
        .unwrap_or(indent + 2);
    let visible_titles = usize::from(!navigation.is_empty()) + usize::from(!commands.is_empty());
    let fixed = 1 + visible_titles + 2;
    let height = (fixed + navigation.len() + commands.len()) as u16;
    (
        widest.min(frame.width.into()) as u16,
        height.min(frame.height),
    )
}

/// How many lines [`Overlay::SyncPreview`] renders for `plan` --- the
/// counting half of `ui::overlay`'s `draw_sync_preview`, which builds the
/// same sections: a header, up to eight rows, an "... and N more" when
/// clipped, and a blank line after every section but the last.
pub(crate) fn sync_preview_lines(plan: &SyncPlan) -> usize {
    fn section(len: usize, trailing_blank: bool) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    fn app() -> App {
        App::new("/nonexistent-project-dir")
    }

    /// The docs pickers' modal fills the frame minus one column per side
    /// and two rows above and below, and the west list's column stays
    /// fixed at 32 --- every extra column a wider terminal adds belongs
    /// to the details pane.
    #[test]
    fn packages_modal_keeps_its_list_column_and_gives_width_to_details() {
        let areas = packages(area());
        assert_eq!(
            areas.popup,
            Rect::new(1, 2, 98, 36),
            "centered, the docs picker's own margins"
        );
        assert_eq!(areas.list.width, PACKAGES_LIST_WIDTH);
        let narrow_details = areas.details.width;

        let wide = packages(Rect::new(0, 0, 140, 40));
        assert_eq!(
            wide.list.width, PACKAGES_LIST_WIDTH,
            "the list keeps its column"
        );
        assert!(
            wide.details.width > narrow_details,
            "every added column goes to the details pane"
        );
    }

    #[test]
    fn docs_picker_modal_margins_and_fixed_list_column() {
        let areas = docs_picker(area());
        assert_eq!(
            areas.popup,
            Rect::new(1, 2, 98, 36),
            "centered, margins 1/2"
        );
        assert_eq!(
            areas.list,
            Rect::new(2, 6, 32, 19),
            "list above, fixed width"
        );
        assert_eq!(
            areas.preview,
            Rect::new(2, 25, 32, 12),
            "preview below, same column"
        );
        assert_eq!(
            areas.details,
            Rect::new(34, 6, 64, 31),
            "details takes the rest"
        );

        // Widening the frame widens only the details column: the list and
        // preview keep their whole geometry.
        let wide = docs_picker(Rect::new(0, 0, 140, 40));
        assert_eq!(wide.list, areas.list);
        assert_eq!(wide.preview, areas.preview);
        assert_eq!(wide.details.width, 104);
    }

    /// Row 1 is fixed-height and halved; row 3 takes whatever remains ---
    /// the invariants every backend's frame depends on.
    #[test]
    fn row1_is_fixed_and_halved_row3_takes_the_rest() {
        let areas = dashboard(&app(), area());
        assert_eq!(
            areas.project.height,
            super::super::panels::INFO_ROWS as u16 + 2
        );
        assert_eq!(areas.project.y, area().y);
        assert_eq!(areas.device.y, areas.project.y);
        assert_eq!(areas.project.height, areas.device.height);
        assert_eq!(
            areas.project.width + areas.device.width,
            area().width,
            "row 1 halves cover the whole width"
        );
        assert_eq!(
            areas.row3.y + areas.row3.height,
            area().y + area().height,
            "row 3 runs to the body's bottom edge"
        );
        assert!(areas.row3.height > 0);
    }

    /// With no panes at all (a bare app, no backend bootstrap) the row is
    /// the placeholder and the split falls back to the historical 60/40.
    #[test]
    fn a_bare_app_shows_the_placeholder_row() {
        let areas = dashboard(&app(), area());
        assert!(matches!(areas.row2, Row2::Placeholder(_)));
        let Row2::Placeholder(rect) = areas.row2 else {
            unreachable!()
        };
        assert_eq!(rect.y, areas.project.y + areas.project.height);
    }

    /// Same app, same body: two calls agree, which is what lets the
    /// hit-tester trust the layout the frame drew.
    #[test]
    fn the_tree_is_deterministic() {
        let app = app();
        assert_eq!(dashboard(&app, area()), dashboard(&app, area()));
    }
}
