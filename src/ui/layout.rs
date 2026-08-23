//! The dashboard's geometry as a pure function of [`App`] state.
//!
//! One definition of where every pane sits, consumed by both the renderer
//! ([`crate::ui::draw`]) and the mouse hit-testing (`crate::app::mouse`):
//! a click must land on exactly the rect the frame drew, so the two cannot
//! each carry their own copy of the splits. Recomputing the tree is a
//! handful of `Layout` solves --- cheap enough to run per frame and per
//! gesture, and free of cached state that a resize would stale.

use ratatui::layout::{Constraint, Layout, Rect};

use crate::app::App;
use crate::backend::Capability;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    fn app() -> App {
        App::new("/nonexistent-project-dir")
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
