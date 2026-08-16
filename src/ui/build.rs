//! Project-panel rendering (the build lifecycle's pane, titled "Project
//! actions"): the project checklist (`Project path`, then `Board`) over
//! a horizontal separator and the lifecycle buttons (`crate::ui::button`),
//! dimmed until both answers exist, with the command state pinned to the
//! pane's last line. Selected rows highlight full-width.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::button::{self, Button};
use super::workspace::label;
use crate::app::{App, Focus};
use crate::build::{BuildPanel, BuildReport};
use crate::ui::{dashboard_focused, pane_block};

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(panel) = &app.build else {
        return;
    };
    let focused = dashboard_focused(app, Focus::Build);
    let block = pane_block("Project actions", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content_end = draw_rows(frame, inner, app, panel);
    draw_state(frame, inner, panel, content_end);
}

/// The command state, pinned to the pane's last line: the live counter
/// while a command runs, the last result once it finishes. Skipped when
/// the rows themselves already fill the pane (a running command's `Stop`
/// box costs three lines) --- the Monitor tab carries the live state then.
fn draw_state(frame: &mut Frame, area: Rect, panel: &BuildPanel, content_end: u16) {
    if area.height < 2 || content_end >= area.bottom() {
        return;
    }
    let line = if let Some(elapsed) = panel.elapsed() {
        Line::from(vec![
            label("state"),
            format!("running · {}", BuildPanel::secs(elapsed)).fg(Color::Cyan),
        ])
    } else if let Some(report) = &panel.last {
        report_line(report)
    } else {
        Line::from(vec![label("state"), "never built".dim()])
    };
    let rect = Rect {
        y: area.bottom() - 1,
        ..area
    };
    frame.render_widget(Paragraph::new(line), rect);
}

fn report_line(report: &BuildReport) -> Line<'static> {
    let what = report.what;
    let (mark, style) = if report.ok {
        ("✓", ratatui::style::Style::new().fg(Color::Green))
    } else {
        ("✗", ratatui::style::Style::new().fg(Color::Red))
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

/// The pane's rows: `Stop` while a command runs and the operation buttons,
/// all stacked in one shared-border group --- bold when both checklist
/// answers (asked in the workspace pane) exist, dim while they wait, the
/// selection highlighting only the button's own row. Returns the y past
/// the last row.
fn draw_rows(frame: &mut Frame, area: Rect, app: &App, panel: &BuildPanel) -> u16 {
    let caps = app.manager.capabilities();
    let actions = panel.actions(&caps);
    let mut y = area.y;
    let mut buttons: Vec<Button> = Vec::new();
    for (position, action) in actions.iter().enumerate() {
        let selected = panel.cursor == position;
        match action {
            crate::build::BuildAction::Stop => {
                buttons.push(Button::new("■ Stop").selected(selected));
            }
            crate::build::BuildAction::Build(kind) => buttons.push(
                Button::new(format!("{} {}", kind_icon(*kind), kind.label()))
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::Flash => buttons.push(
                Button::new("⇧ Flash")
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::Menuconfig => buttons.push(
                Button::new("✎ Menuconfig")
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::UpdateZephyr => buttons.push(
                Button::new("↻ Update Zephyr")
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::SdkList => buttons.push(
                Button::new("≡ SDK List")
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
        }
    }
    y = button::render_stack(frame, area, y, &buttons);
    y
}

/// The button glyph for a lifecycle kind: one glance tells the actions
/// apart, the way `Stop`'s `■` already does.
fn kind_icon(kind: crate::backend::BuildKind) -> &'static str {
    match kind {
        crate::backend::BuildKind::Build => "▶",
        crate::backend::BuildKind::Clean => "×",
        crate::backend::BuildKind::Rebuild => "⟳",
    }
}
