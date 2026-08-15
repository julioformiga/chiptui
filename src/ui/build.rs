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

    let [header, list] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(inner);

    draw_header(frame, header, panel);
    draw_actions(frame, list, app, panel, focused);
}

/// Project + board + last-result lines. The project line says where every
/// command runs (`cwd` --- inherited from the launch directory --- or
/// `picked`, a session choice from the projects folder); the board's origin
/// is the other nuance worth a word --- `from build/` (whatever `west`
/// configured, which the panel neither chose nor wrote) or `picked`
/// (session-only, `SPEC.md` §10) --- never left to guesswork.
fn draw_header(frame: &mut Frame, area: Rect, panel: &BuildPanel) {
    use crate::build::BoardOrigin;

    let project_line = Line::from(vec![
        label("project"),
        Span::raw(" "),
        shorten_start(
            &panel.root.display().to_string(),
            (area.width as usize).saturating_sub(16),
        )
        .fg(Color::Green)
        .bold(),
        Span::raw(" "),
        panel.project_origin.label().dim(),
    ]);

    let board_line = match (&panel.board, panel.is_busy()) {
        (
            Some(
                choice @ crate::build::BoardChoice {
                    origin: BoardOrigin::Picked,
                    ..
                },
            ),
            _,
        ) => Line::from(vec![
            label("board"),
            choice.name.clone().fg(Color::Green).bold(),
            Span::raw(" "),
            format!("picked · dir {}", panel.build_dir).dim(),
        ]),
        (Some(choice), _) => Line::from(vec![
            label("board"),
            choice.name.clone().fg(Color::Green).bold(),
            Span::raw(" "),
            format!("from {}/", panel.build_dir).dim(),
        ]),
        (None, true) => Line::from(vec![
            label("board"),
            format!("— (this build configures one · dir {})", panel.build_dir).dim(),
        ]),
        (None, false) => Line::from(vec![
            label("board"),
            "none".fg(Color::Yellow),
            Span::raw(" "),
            "pick a target below, or the first build needs -b BOARD".dim(),
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
        Paragraph::new(vec![project_line, board_line, last_line]).wrap(Wrap { trim: false }),
        area,
    );
}

fn report_line(report: &BuildReport) -> Line<'static> {
    let what = report.what;
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

/// The action list: `Stop` while a command runs, then the lifecycle entries
/// each with the command it would run today (board, build directory and
/// tool-override all reflected --- what you see is what runs), then `Flash`
/// and `Board` under their capabilities.
fn draw_actions(frame: &mut Frame, area: Rect, app: &App, panel: &BuildPanel, focused: bool) {
    let backend = app.manager.backend();
    let caps = app.manager.capabilities();
    let mut items = Vec::new();

    for action in panel.actions(&caps) {
        let item = match action {
            crate::build::BuildAction::Stop => Line::from(vec![
                Span::styled("■ ", Style::new().fg(Color::Red)),
                "Stop".bold(),
                "   cancel the running command".dim(),
            ]),
            crate::build::BuildAction::Build(kind) => {
                let command = backend
                    .and_then(|backend| panel.command(kind, backend))
                    .map(|command| command.to_string())
                    .unwrap_or_else(|| "not available".to_string());
                Line::from(vec![
                    Span::raw("  "),
                    kind.label().bold(),
                    Span::raw("  "),
                    Span::styled(
                        shorten_start(&command, width_for(area.width)),
                        Style::new().dim(),
                    ),
                ])
            }
            crate::build::BuildAction::Flash => {
                let command = backend
                    .and_then(|backend| panel.flash_command(backend))
                    .map(|command| command.to_string())
                    .unwrap_or_else(|| "not available".to_string());
                Line::from(vec![
                    Span::raw("  "),
                    "Flash".bold(),
                    Span::raw("  "),
                    Span::styled(
                        shorten_start(&command, width_for(area.width)),
                        Style::new().dim(),
                    ),
                ])
            }
            crate::build::BuildAction::Menuconfig => {
                let command = backend
                    .and_then(|backend| panel.menuconfig_command(backend))
                    .map(|command| command.to_string())
                    .unwrap_or_else(|| "not available".to_string());
                Line::from(vec![
                    Span::raw("  "),
                    "Menuconfig".bold(),
                    Span::raw("  "),
                    Span::styled(
                        shorten_start(&command, width_for(area.width)),
                        Style::new().dim(),
                    ),
                ])
            }
            crate::build::BuildAction::BuildDir => Line::from(vec![
                Span::raw("  "),
                "Dir".bold(),
                Span::raw("  "),
                Span::styled(
                    format!(
                        "{}  (choose or type another: west build -d)",
                        panel.build_dir
                    ),
                    Style::new().dim(),
                ),
            ]),
            crate::build::BuildAction::Board => Line::from(vec![
                Span::raw("  "),
                "Board".bold(),
                Span::raw("  "),
                Span::styled(
                    "choose the target (west boards, session-only)",
                    Style::new().dim(),
                ),
            ]),
            crate::build::BuildAction::Project => {
                let hint = if crate::backend::zephyr::projects::is_buildable(&panel.root) {
                    format!(
                        "{}  (the picker lists the projects folder)",
                        panel.root.display()
                    )
                } else {
                    "none here — pick one before building".to_string()
                };
                Line::from(vec![
                    Span::raw("  "),
                    "Project".bold(),
                    Span::raw("  "),
                    Span::styled(hint, Style::new().dim()),
                ])
            }
        };
        items.push(ListItem::new(item));
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
