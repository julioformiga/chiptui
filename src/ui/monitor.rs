//! Row 3's Monitor tab: whichever live process output the user last asked
//! for --- a running/just-finished flash (`esptool`) command, or (once wired
//! up) a live device serial session --- rendered in one place instead of a
//! separate dialog (`SPEC.md` §11).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, Focus, MonitorSource};
use crate::backend::Capability;
use crate::flash::{OptionsField, RunState};
use crate::ui::flash::{field_label, field_value};
use crate::ui::{content_style, dashboard_focused, pane_block};

/// Chip + offset, always meaningful for `WriteFlash`/`VerifyFlash` on the
/// recap above the console --- the options screen's full field list
/// (`FlashPanel::options_fields`) also covers flash mode/freq/size/extra
/// flags, more detail than a short recap needs.
const RECAP_FIELDS: &[OptionsField] = &[OptionsField::Chip, OptionsField::Offset];

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let focused = dashboard_focused(app, Focus::Logs);
    match app.monitor_source {
        MonitorSource::Flash => draw_flash_output(frame, area, app, focused),
        MonitorSource::Device => draw_device_monitor(frame, area, app, focused),
    }
}

/// Placeholder body until the live device session (a later change) exists.
fn draw_device_monitor(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let block = pane_block("Monitor", focused);

    if app.device_monitor_process.is_some() || !app.device_monitor_output.is_empty() {
        let inner_height = block.inner(area).height.max(1) as usize;
        let console_budget = inner_height;

        let mut console: Vec<Line> = app
            .device_monitor_output
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect();

        if console.is_empty() {
            console.push(Line::from("(connected)".dim()));
        }

        let visible_start = console.len().saturating_sub(console_budget);
        let lines: Vec<Line> = console.split_off(visible_start);

        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let message = if app.manager.capabilities().contains(Capability::Monitor) {
        "not connected — press 'm' to start".to_string()
    } else {
        let backend = app
            .manager
            .selected_kind()
            .map_or("this backend".to_string(), |kind| kind.to_string());
        format!("{backend}: monitor not available")
    };

    frame.render_widget(
        Paragraph::new(message.dim())
            .block(block)
            .style(content_style(focused)),
        area,
    );
}

/// Streamed `esptool` output, moved here from the Flash dialog's former
/// `FlashScreen::Output` screen --- same content, different home.
fn draw_flash_output(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let Some(flash) = app.flash.as_ref() else {
        draw_device_monitor(frame, area, app, focused);
        return;
    };

    let action = flash.selected_action();
    let status = match &flash.state {
        RunState::Idle => "",
        RunState::Running => " (running…)",
        RunState::Succeeded => " (done)",
        RunState::Failed(_) => " (failed)",
    };

    // `WriteFlash`/`VerifyFlash` get a short read-only recap of what is
    // actually running above the console --- `SPEC.md` §15's "never hide
    // what is running behind a paraphrase" applies here too, not just to the
    // confirmation overlay. Other actions have nothing to recap, so the
    // block title carries the status instead and the console gets the whole
    // pane.
    let (block, mut lines) = if action.needs_firmware() {
        let block = pane_block(action.label(), focused);
        let rule = "─".repeat(block.inner(area).width as usize);

        let mut lines: Vec<Line> = RECAP_FIELDS
            .iter()
            .map(|field| {
                Line::from(vec![
                    Span::styled(format!("{:<10}", field_label(*field)), Style::new().dim()),
                    Span::raw(field_value(flash, *field)),
                ])
            })
            .collect();
        if let Some(firmware) = flash.selected_firmware_path() {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<10}", "firmware"), Style::new().dim()),
                Span::raw(firmware.display().to_string()),
            ]));
        }
        lines.push(Line::from(rule.dim()));
        lines.push(Line::from(format!("console{status}").dim()));
        (block, lines)
    } else {
        (pane_block(&format!("Monitor{status}"), focused), Vec::new())
    };

    // The console always follows the tail rather than offering manual
    // scroll, so a long `write-flash` run is legible while it streams
    // instead of sitting on whatever fit on screen when the command started.
    let inner_height = block.inner(area).height.max(1) as usize;
    let console_budget = inner_height.saturating_sub(lines.len());

    let mut console: Vec<Line> = flash
        .output
        .iter()
        .map(|line| Line::from(line.clone()))
        .collect();
    // The header already reads "(done)"/"(failed)": a banner line here would
    // just be a second, redundant announcement mixed into the command's real
    // output.
    if let RunState::Failed(error) = &flash.state {
        console.push(Line::from(""));
        console.push(Line::from(error.clone().fg(Color::Red)));
    }
    if console.is_empty() {
        console.push(Line::from("(no output yet)".dim()));
    }

    let visible_start = console.len().saturating_sub(console_budget);
    lines.extend(console.split_off(visible_start));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}
