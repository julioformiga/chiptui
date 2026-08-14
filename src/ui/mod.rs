//! Rendering.
//!
//! Rendering is a pure function of [`App`] state (plus the log viewport height,
//! which the renderer publishes back so scrolling matches what is on screen).
//! Colors come from the terminal's own 16-color palette --- `AGENTS.md` asks
//! for terminal-native output rather than an imposed theme.

mod files;
mod flash;
mod monitor;
mod overlay;
mod panels;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use crate::app::{App, Focus, LogTab, View};

/// Below this the panes cannot be rendered legibly.
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 14;

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
/// placeholder. Row 3: a one-line Log/Monitor tab strip over the tab's body,
/// full width (`SPEC.md` §11).
///
/// Row 1's height adapts to its content: the taller of the two info panes
/// plus borders. Both are informational (never focused), and `memory:` shares
/// the `flash id:` line, so the row starts compact and only grows when device
/// details accumulate.
fn draw_dashboard(frame: &mut Frame, body: Rect, app: &mut App) {
    let half_width = (body.width / 2).max(1) as usize;
    let project_n = panels::project_content(app, half_width).len();
    let device_n = panels::device_content(app).len();
    let info_height = project_n.max(device_n).max(1) as u16 + 2;

    let [row1, rest] =
        Layout::vertical([Constraint::Length(info_height), Constraint::Min(0)]).areas(body);
    let [row2, row3] =
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(rest);
    let [project, device] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(row1);

    panels::draw_project(frame, project, app);
    panels::draw_detection(frame, device, app);

    // Row 2 shows the file browser whenever there is a browser to draw ---
    // for every backend once `maybe_scan_devices` has run, since the local
    // pane stands on its own. `files::draw` decides what the right half
    // shows (device files under `Capability::Filesystem`, a placeholder
    // otherwise); the full-width placeholder remains for the window before
    // the browser exists at all.
    if app.browser.is_some() {
        files::draw(frame, row2, app);
    } else {
        panels::draw_no_filesystem(frame, row2, app);
    }

    let [tabs, tab_body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(row3);
    panels::draw_log_tabs(frame, tabs, app);
    match app.log_tab {
        LogTab::Log => panels::draw_logs(frame, tab_body, app),
        LogTab::Monitor => monitor::draw(frame, tab_body, app),
    }
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
    let project = app.manager.name().unwrap_or("--");
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

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();
    for (key, label) in app.shortcuts() {
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
    let border = if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let title_style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().dim()
    };

    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), title_style))
}
