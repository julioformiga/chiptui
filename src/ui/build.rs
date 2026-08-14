//! Build-panel rendering: a status header (board, last result) over a
//! navigable action list whose rows show the literal command each entry
//! would run --- the same never-paraphrase rule as the flash confirms
//! (`SPEC.md` §15), applied before the command starts.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus};
use crate::backend::BuildKind;
use crate::build::{BuildPanel, BuildReport};
use crate::ui::{content_style, dashboard_focused, pane_block};

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(panel) = &app.build else {
        return;
    };
    let focused = dashboard_focused(app, Focus::Build);
    let block = pane_block("Build", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [header, list] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);

    draw_header(frame, header, panel);
    draw_actions(frame, list, app, panel, focused);
}

/// Board + last-result lines. Two lines, never more: the board's origin is
/// the one nuance worth a word (`from build/` = whatever `west` configured,
/// which the panel neither chose nor wrote --- `SPEC.md` §10's "board
/// selection must not silently modify project configuration", read-side).
fn draw_header(frame: &mut Frame, area: Rect, panel: &BuildPanel) {
    let board_line = match (&panel.board, panel.is_busy()) {
        (Some(board), _) => Line::from(vec![
            label("board"),
            board.clone().fg(Color::Green).bold(),
            Span::raw(" "),
            "from build/".dim(),
        ]),
        (None, true) => Line::from(vec![label("board"), "— (this build configures one)".dim()]),
        (None, false) => Line::from(vec![
            label("board"),
            "none".fg(Color::Yellow),
            Span::raw(" "),
            "the first build needs one: west build -b BOARD".dim(),
        ]),
    };

    let last_line = if let Some(elapsed) = panel.elapsed() {
        Line::from(vec![
            label("state"),
            format!("running · {}", BuildPanel::secs(elapsed)).fg(Color::Cyan),
        ])
    } else if let Some(report) = &panel.last {
        report_line(report)
    } else {
        Line::from(vec![label("state"), "never built".dim()])
    };

    frame.render_widget(
        Paragraph::new(vec![board_line, last_line]).wrap(Wrap { trim: false }),
        area,
    );
}

fn report_line(report: &BuildReport) -> Line<'static> {
    let what = report.kind.label();
    let (mark, style) = if report.ok {
        ("✓", Style::new().fg(Color::Green))
    } else {
        ("✗", Style::new().fg(Color::Red))
    };
    let outcome = if report.ok {
        format!("{what} ok in {}", BuildPanel::secs(report.duration))
    } else {
        format!("{what} failed")
    };
    Line::from(vec![
        label("last"),
        Span::styled(format!("{mark} {outcome}"), style),
        Span::raw(" "),
        format!("{:02}:{:02}", report.at.hour(), report.at.minute()).dim(),
    ])
}

fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text:<6}"), Style::new().dim())
}

/// The action list: `Stop` first while running, then the lifecycle entries,
/// each with the command it would run today (board, build directory and
/// tool-override all reflected --- what you see is what runs).
fn draw_actions(frame: &mut Frame, area: Rect, app: &App, panel: &BuildPanel, focused: bool) {
    let backend = app.manager.backend();
    let mut items = Vec::new();

    if panel.is_busy() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("■ ", Style::new().fg(Color::Red)),
            "Stop".bold(),
            "   cancel the running command".dim(),
        ])));
    }

    for kind in BuildKind::ALL {
        let command = backend
            .and_then(|backend| panel.command(*kind, backend))
            .map(|command| command.to_string())
            .unwrap_or_else(|| "not available".to_string());
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            kind.label().bold(),
            Span::raw("  "),
            Span::styled(
                shorten_start(&command, width_for(area.width)),
                Style::new().dim(),
            ),
        ])));
    }

    let mut state = ListState::default().with_selected(Some(panel.cursor));
    frame.render_stateful_widget(
        List::new(items)
            .style(content_style(focused))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

/// Width left for a command after a label column.
fn width_for(width: u16) -> usize {
    (width as usize).saturating_sub(10).max(8)
}

/// Shortens from the left: a command's tail (flags, board) is its identity;
/// the leading `west` is the part anyone can guess.
fn shorten_start(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let tail: String = text.chars().skip(length - (max - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_shorten_from_the_left() {
        assert_eq!(
            shorten_start("west build -b board", 30),
            "west build -b board"
        );
        // Shortened from the left, keeping the tail (flags, board) and the
        // exact width budget.
        let shortened = shorten_start("west build --pristine=always -b b", 16);
        assert!(shortened.starts_with('…'));
        assert!(shortened.ends_with("-b b"));
        assert_eq!(shortened.chars().count(), 16);
        assert_eq!(shorten_start("west build", 1), "…");
    }
}
