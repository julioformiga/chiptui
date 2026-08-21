//! Project-panel rendering (the build lifecycle's pane, titled "Project
//! actions"): the project checklist (`Project path`, then `Board`) over
//! a horizontal separator and the lifecycle buttons (`crate::ui::button`),
//! dimmed until both answers exist, with the command state pinned to the
//! pane's last line. Selected rows highlight full-width. While a command
//! runs, the pane's footer sits directly under the stack: the state on the
//! left half, `Stop` as its own half-width button box on the right --- side
//! by side, never one pushing the other, and never a row of the stack. The
//! footer's three rows are reserved even while idle, so the pane's height
//! never changes when a command starts.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::button::{self, Button};
use super::workspace::label;
use crate::app::{App, Focus};
use crate::build::{BuildPanel, BuildReport};
use crate::ui::{Palette, dashboard_focused, pane_block};

pub fn draw(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let Some(panel) = &app.build else {
        return;
    };
    let focused = dashboard_focused(app, Focus::Build);
    let block = pane_block("Actions", focused, palette);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let footer_top = draw_rows(frame, inner, app, panel, palette);
    draw_state(frame, inner, panel, footer_top, palette);
}

/// The command state: the live counter while a command runs, the last
/// result once it finishes, never built before the first one. It rides
/// the reserved footer's label row (see [`draw_rows`]) on the same line
/// whether or not the `Stop` box is showing --- beside the box, on the
/// left half, while one runs; full-width over the empty reservation
/// otherwise. Nothing in the pane moves when a command starts or ends.
fn draw_state(
    frame: &mut Frame,
    area: Rect,
    panel: &BuildPanel,
    footer_top: u16,
    palette: Palette,
) {
    if area.height < 2 || footer_top + 1 >= area.bottom() {
        return;
    }
    let line = if let Some(elapsed) = panel.elapsed() {
        let text = match panel.progress() {
            // The tool's own progress (ninja's step counter, or an esptool
            // percentage from a runner that shells out to it) --- the
            // command's label instead of the generic "running", since the
            // count alone does not say what it counts.
            Some(progress) => format!(
                "{} · {}",
                panel.running_label().unwrap_or("running"),
                progress.render()
            ),
            None => format!("running · {}", BuildPanel::secs(elapsed)),
        };
        Line::from(vec![label("state", palette), text.fg(palette.accent)])
    } else if let Some(report) = &panel.last {
        report_line(report, palette)
    } else {
        Line::from(vec![
            label("state", palette),
            "never built".fg(palette.muted),
        ])
    };
    let rect = Rect {
        y: footer_top + 1,
        width: if panel.is_busy() {
            area.width / 2
        } else {
            area.width
        },
        ..area
    };
    frame.render_widget(Paragraph::new(line), rect);
}

fn report_line(report: &BuildReport, palette: Palette) -> Line<'static> {
    let what = report.what;
    let (mark, style) = if report.ok {
        ("✓", ratatui::style::Style::new().fg(palette.success))
    } else {
        ("✗", ratatui::style::Style::new().fg(palette.error))
    };
    let outcome = if report.ok {
        format!("{what} ok in {}", BuildPanel::secs(report.duration))
    } else {
        format!("{what} failed")
    };
    Line::from(vec![
        label("last", palette),
        Span::styled(format!("{mark} {outcome}"), style),
        Span::raw(" "),
        format!("{:02}:{:02}", report.at.hour(), report.at.minute()).fg(palette.muted),
    ])
}

/// The pane's rows: the operation buttons stacked in one shared-border
/// group --- bold when both checklist answers (asked in the workspace
/// pane) exist, dim while they wait, the selection highlighting only the
/// button's own row --- and the three-row footer under the stack's bottom
/// rule, reserved whether or not a command runs: `Stop` as its own
/// half-width box on the right half while one does (invisible otherwise,
/// its space waiting), the state line on the left half. Reserving the
/// rows is what keeps the pane's height --- and every row's place ---
/// constant when a command starts or ends. Returns the footer's top row.
fn draw_rows(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    panel: &BuildPanel,
    palette: Palette,
) -> u16 {
    let caps = app.manager.capabilities();
    let actions = panel.actions(&caps);
    // `Stop` trails the list exactly while a command runs; the stack shows
    // the buttons above it, and `Stop` itself becomes the footer's box.
    let stop = panel.is_busy() && matches!(actions.last(), Some(crate::build::BuildAction::Stop));
    let y = area.y;
    let mut buttons: Vec<Button> = Vec::new();
    let mains = &actions[..actions.len() - usize::from(stop)];
    for (position, action) in mains.iter().enumerate() {
        let selected = panel.cursor == position;
        match action {
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
                Button::new("↻ Update Zephyr/SDK")
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::InstallZephyr => buttons.push(
                Button::new("⇩ Install Zephyr")
                    .enabled(app.build_action_enabled(*action))
                    .selected(selected),
            ),
            crate::build::BuildAction::Stop => unreachable!("Stop is drawn as the footer box"),
        }
    }
    // The footer sits directly under the stack's bottom rule --- no blank
    // row between Flash and Stop --- unless the pane is too short for
    // both, when it pins to the bottom instead and the stack clips above
    // it (the box carries the cursor while a command runs; a clipped
    // button row comes back when the command ends).
    let stack_end = y + button::stack_height(&buttons);
    let footer_top = stack_end.min(area.bottom().saturating_sub(3)).max(area.y);
    let stack_area = Rect {
        height: footer_top.saturating_sub(area.y),
        ..area
    };
    button::render_stack(frame, stack_area, y, &buttons, palette);
    if stop {
        // The right half of the footer: the same stacked-button widget,
        // one button of its own, sharing its label row with the state.
        let half = area.width / 2;
        let corner = Rect {
            x: area.x + half,
            width: area.width - half,
            y: footer_top,
            height: area.bottom().saturating_sub(footer_top),
        };
        let selected = panel.cursor == actions.len() - 1;
        button::render_stack(
            frame,
            corner,
            footer_top,
            &[Button::new("■ Stop").selected(selected)],
            palette,
        );
    }
    footer_top
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
