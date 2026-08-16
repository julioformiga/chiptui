//! Rendering.
//!
//! Rendering is a pure function of [`App`] state (plus the log viewport height,
//! which the renderer publishes back so scrolling matches what is on screen).
//! Colors come from the terminal's own 16-color palette --- `AGENTS.md` asks
//! for terminal-native output rather than an imposed theme.

mod build;
mod button;
mod files;
mod flash;
pub mod home;
mod monitor;
mod overlay;
mod panels;
mod workspace;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::app::{App, Focus, LogTab, View};

/// Below this the panes cannot be rendered legibly.
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 14;

/// Frames of the shared "something is running" spinner, animated off
/// [`App::ticks`] (one frame per tick). Used by the file panes' waits, the
/// board picker's fetch, and the Monitor tab's live status.
pub(crate) const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        return;
    }

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, header, app);

    // The flash view is a dialog layered over the dashboard, never a
    // full-screen replacement, so the project/device/files panes stay
    // visible (and, via `pane`'s `View::Dashboard` check below, visibly
    // dimmed) while esptool commands run.
    draw_dashboard(frame, body, app);
    if app.view == View::Flash {
        draw_flash_dialog(frame, body, app);
    }

    draw_footer(frame, footer, app);
    overlay::draw(frame, area, app);
}

/// The flash action menu/options, sized to its content (like
/// `overlay::draw_confirm`/`draw_device_picker`) and centered over `body`
/// with a `Clear` behind it so the dashboard shows through around the edges
/// --- a real dialog, not a near-fullscreen replacement. Running an action
/// closes this dialog (`App::show_flash_in_monitor`), so it never needs to
/// size itself for streamed output.
fn draw_flash_dialog(frame: &mut Frame, body: Rect, app: &App) {
    let Some(flash) = &app.flash else { return };
    let (width, height) = flash::dialog_size(flash);
    let popup = centered(body, width, height);
    frame.render_widget(Clear, popup);
    flash::draw(frame, popup, app);
}

/// Centers a `width`×`height` box inside `area`, shrinking to fit. Shared
/// with `overlay::draw` --- every modal in this app sizes itself off its own
/// content rather than a fraction of the screen.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    popup
}

/// Row 1: Project | Device, split evenly. Row 2: the dual-pane file browser
/// when the backend has `Capability::Filesystem`, else a full-width
/// placeholder. Row 3: one bordered pane whose top border carries the
/// Log/Monitor tab strip over the selected tab's body, full width
/// (`SPEC.md` §11).
///
/// Row 1's height adapts to its content: the taller of the two info panes
/// plus borders. Both are informational (never focused), so the row starts
/// compact and only grows when device details accumulate.
fn draw_dashboard(frame: &mut Frame, body: Rect, app: &mut App) {
    let half_width = (body.width / 2).max(1) as usize;
    let project_n = panels::project_content(app, half_width).len();
    let device_n = panels::device_content(app).len();
    let info_height = project_n.max(device_n).max(1) as u16 + 2;

    let [row1, rest] =
        Layout::vertical([Constraint::Length(info_height), Constraint::Min(0)]).areas(body);
    // Row 2 leans on its content when the workspace/project panes claim
    // it: their stacked button groups (checklist rows, a separator, a rule
    // per button edge, the pinned state line) are the tallest content on
    // the dashboard, so the row is sized to fit them and the log pane
    // (which scrolls) takes the remainder. The browser keeps the
    // historical 60/40 split.
    let [row2, row3] = if app.workspace_pane_visible() {
        let needed = row2_content_height(app)
            .saturating_add(2) // the pane's borders (the state line is content, already counted)
            .min(rest.height.saturating_sub(3).max(1));
        Layout::vertical([Constraint::Length(needed), Constraint::Min(0)]).areas(rest)
    } else {
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(rest)
    };
    let [project, device] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(row1);

    panels::draw_project(frame, project, app);
    panels::draw_detection(frame, device, app);

    // Row 2 belongs to whichever panes the backend's capabilities give it:
    // the dual-pane file browser under `Capability::Filesystem`, the
    // workspace+build pair for a backend that builds without a device
    // filesystem (`SPEC.md` §11), and a placeholder only in the window
    // before the panes exist at all.
    if app.workspace_pane_visible() {
        workspace::draw_row(frame, row2, app);
    } else if app.browser.is_some() {
        files::draw(frame, row2, app);
    } else {
        panels::draw_no_filesystem(frame, row2, app);
    }

    // Row 3 is one bordered pane for the whole width: the Log/Monitor tab
    // strip lives on the pane's own top border (`SPEC.md` §11), like the
    // Ratatui `Tabs` example, and the selected tab's body fills the pane.
    match app.log_tab {
        LogTab::Log => panels::draw_logs(frame, row3, app),
        LogTab::Monitor => monitor::draw(frame, row3, app),
    }
    panels::draw_log_tabs(frame, row3, app);
}

/// The minimum rows the workspace pane's embedded file list gets, past the
/// checklist and its header line --- enough to show a handful of entries
/// without scrolling. Row 2 is sized to fit exactly this much (like the
/// checklist rows above it); a taller terminal gives the remainder to row 3
/// (the log/monitor pane), the same trade-off row 2 already makes today.
const MIN_FILES_ROWS: u16 = 6;

/// The taller row-2 pane's inner content height: the workspace pane's
/// checklist rows (plus an invalid location's wrapped reason), a separator,
/// the file-list header and its minimum rows on one side; the project
/// pane's stacked button group --- one row per button, one rule at each
/// edge and one divider between each pair --- on the other.
fn row2_content_height(app: &App) -> u16 {
    let caps = app.manager.capabilities();
    let workspace = app.workspace.as_ref().map_or(0, |panel| {
        let checklist = panel.actions(&caps).len() as u16;
        let invalid = if panel.invalid.is_some() { 4 } else { 0 };
        let separator = 1;
        let files_header = 1;
        checklist + invalid + separator + files_header + MIN_FILES_ROWS
    });
    let build = app.build.as_ref().map_or(0, |panel| {
        // The stacked group plus a three-row footer, reserved whether or
        // not the `Stop` box is showing (`Stop` is appended to the list,
        // never a stacked row, so the group itself never changes size):
        // the pane's height must not change when a command starts.
        let mains = panel.actions(&caps).len() - usize::from(panel.is_busy());
        (2 * mains + 1 + 3) as u16
    });
    workspace.max(build)
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    let message = Paragraph::new(vec![
        Line::from("Terminal too small".bold()),
        Line::from(format!(
            "need at least {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
            area.width, area.height
        )),
    ])
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: true });
    frame.render_widget(message, area);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let project = app.header_project();
    let backend = app
        .manager
        .selected_kind()
        .map_or("none", |kind| kind.display_name());

    let header = Line::from(vec![
        Span::styled(
            " ChipTUI ",
            Style::new().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::raw(" "),
        Span::styled("project ", Style::new().dim()),
        Span::raw(project),
        Span::styled("  backend ", Style::new().dim()),
        Span::raw(backend),
        Span::styled("  device ", Style::new().dim()),
        Span::styled(
            app.devices.summary(),
            if app.devices.selected().is_some() {
                Style::new().fg(Color::Green)
            } else {
                Style::new().dim()
            },
        ),
    ]);

    frame.render_widget(Paragraph::new(header), area);
}

/// The contextual shortcut line.
///
/// More hints than columns is normal on a narrow terminal, and the two that
/// must survive are the last ones (`?` help, `q` quit --- the way out and
/// the way to the rest). So hints are dropped whole, from the *middle*,
/// rather than letting the line truncate mid-word: a cut-off " q  qui" is
/// worse than one fewer hint.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    const KEEP_LAST: usize = 2;

    let mut hints = app.shortcuts();
    let width = |hints: &[(&str, &str)]| -> usize {
        hints
            .iter()
            .map(|(key, label)| key.chars().count() + label.chars().count() + 5)
            .sum()
    };
    while width(&hints) > area.width as usize && hints.len() > KEEP_LAST {
        hints.remove(hints.len() - KEEP_LAST - 1);
    }

    let mut spans = Vec::new();
    for (key, label) in hints {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().bg(Color::DarkGray),
        ));
        spans.push(Span::styled(format!(" {label}  "), Style::new().dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Whether `focus` currently has the user's attention on the dashboard ---
/// false whenever the flash dialog or another overlay has it instead, in
/// which case both a pane's border (`pane_block`) and its content
/// (`content_style`) should read as dimmed.
fn dashboard_focused(app: &App, focus: Focus) -> bool {
    app.view == View::Dashboard && app.focus == focus && app.overlay.is_none()
}

/// Style for a dashboard pane's content: unchanged when focused, dimmed
/// otherwise. Ratatui has no dedicated "darken the background" primitive ---
/// a terminal buffer has no compositing/alpha to blend against, so this is
/// the closest equivalent: `Modifier::DIM` set at the widget level, which
/// cascades onto every already-colored `Span` drawn inside instead of
/// requiring each one to know about focus.
fn content_style(focused: bool) -> Style {
    if focused {
        Style::default()
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

/// A bordered block that shows whether it holds focus.
fn pane_block(title: &str, focused: bool) -> Block<'static> {
    pane_border(focused).title(title_span(title, focused))
}

/// An untitled bordered block that shows whether it holds focus: row 3's
/// pane, whose border row belongs to the Log/Monitor tab strip (with the
/// active tab's status at its right --- see `panels::draw_log_tabs`).
pub(crate) fn pane_border(focused: bool) -> Block<'static> {
    let border = if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
}

fn title_span(title: &str, focused: bool) -> Line<'static> {
    let title_style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().dim()
    };
    Line::from(Span::styled(format!(" {title} "), title_style))
}

/// The discreet one-column scrollbar shared by the Log and Monitor panes:
/// thin dim track, slightly brighter thumb, no arrow heads. Drawn just inside
/// `inner`'s right edge --- callers reserve that column (the Log pane shrinks
/// its wrap width; the Monitor pane pads its block) so wrapped text never
/// reflows when the bar appears. `content` and `viewport` are visual
/// (post-wrap) rows; `position` is the first visible row.
pub(crate) fn draw_scrollbar(
    frame: &mut Frame,
    inner: Rect,
    content: usize,
    viewport: usize,
    position: usize,
) {
    if content <= viewport || inner.width == 0 || inner.height == 0 {
        return;
    }
    let top = position.min(content - viewport);
    let mut state = ScrollbarState::new(content)
        .viewport_content_length(viewport)
        .position(top);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .thumb_symbol("┃")
        .track_style(Style::new().fg(Color::DarkGray))
        .thumb_style(Style::new().fg(Color::Gray));
    frame.render_stateful_widget(
        bar,
        Rect {
            x: inner.right() - 1,
            y: inner.y,
            width: 1,
            height: inner.height,
        },
        &mut state,
    );
}
