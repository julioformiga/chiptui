//! Rendering.
//!
//! Rendering is a pure function of [`App`] state (plus the log viewport height,
//! which the renderer publishes back so scrolling matches what is on screen).
//! Colors come from the terminal's own 16-color palette --- `AGENTS.md` asks
//! for terminal-native output rather than an imposed theme.

mod files;
mod overlay;
mod panels;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use crate::app::{App, Focus, View};

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

    match app.view {
        View::Dashboard => draw_dashboard(frame, body, app),
        // The browser needs the full width; the log stays one key away.
        View::Files => files::draw(frame, body, app),
    }

    draw_footer(frame, footer, app);
    overlay::draw(frame, area, app);
}

fn draw_dashboard(frame: &mut Frame, body: Rect, app: &mut App) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body);
    let [project, detection] =
        Layout::vertical([Constraint::Length(9), Constraint::Min(3)]).areas(left);
    let [capabilities, logs] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);

    panels::draw_project(frame, project, app);
    panels::draw_detection(frame, detection, app);
    panels::draw_capabilities(frame, capabilities, app);
    panels::draw_logs(frame, logs, app);
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
            Style::new().fg(Color::Black).bg(Color::DarkGray),
        ));
        spans.push(Span::styled(format!(" {label}  "), Style::new().dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A bordered block for a dashboard pane.
fn pane(title: &str, focus: Focus, app: &App) -> Block<'static> {
    pane_block(title, app.focus == focus && app.overlay.is_none())
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

/// A ten-cell confidence meter, e.g. `████████░░ 0.83`.
fn confidence_bar(confidence: f32) -> String {
    const CELLS: usize = 10;
    let filled = (confidence * CELLS as f32).round().clamp(0.0, CELLS as f32) as usize;
    format!(
        "{}{} {confidence:.2}",
        "█".repeat(filled),
        "░".repeat(CELLS - filled)
    )
}

/// Color scale for a confidence value, matching the detection thresholds.
fn confidence_color(confidence: f32) -> Color {
    if confidence >= crate::project::AUTO_CONFIDENCE {
        Color::Green
    } else if confidence >= crate::project::MIN_CONFIDENCE {
        Color::Yellow
    } else {
        Color::DarkGray
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_bar_spans_the_full_range() {
        assert_eq!(confidence_bar(0.0), "░░░░░░░░░░ 0.00");
        assert_eq!(confidence_bar(1.0), "██████████ 1.00");
        assert!(confidence_bar(0.5).starts_with("█████░"));
    }

    #[test]
    fn confidence_color_follows_the_detection_thresholds() {
        assert_eq!(confidence_color(0.95), Color::Green);
        assert_eq!(confidence_color(0.45), Color::Yellow);
        assert_eq!(confidence_color(0.05), Color::DarkGray);
    }
}
