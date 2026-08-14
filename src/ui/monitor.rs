//! Row 3's Monitor tab: whichever live process output the user last asked
//! for --- a running/just-finished flash (`esptool`) command, a backend
//! build command (`west build`), or (once wired up) a live device serial
//! session --- rendered in one place instead of a separate dialog
//! (`SPEC.md` §11).

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
        MonitorSource::Run => draw_run_output(frame, area, app, focused),
        MonitorSource::Build => draw_build_output(frame, area, app, focused),
    }
}

/// Streamed build-command output (`west build`), mirroring the flash
/// output's shape: the panel's own header carries board/state, so this pane
/// is just the console, tail-following like the others.
fn draw_build_output(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let Some(panel) = app.build.as_ref() else {
        draw_device_monitor(frame, area, app, focused);
        return;
    };

    let status = if panel.is_busy() {
        " (running…)"
    } else if panel.last.as_ref().is_some_and(|report| report.ok) {
        " (done)"
    } else if panel.last.is_some() {
        " (failed)"
    } else {
        ""
    };

    let block = pane_block(&format!("Build{status}"), focused);
    let inner_height = block.inner(area).height.max(1) as usize;

    let mut console: Vec<Line> = panel
        .output
        .iter()
        .map(|line| Line::from(line.clone()))
        .collect();
    if console.is_empty() {
        console.push(Line::from("(no output yet)".dim()));
    }

    let visible_start = console.len().saturating_sub(inner_height);
    let lines: Vec<Line> = console.split_off(visible_start);

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
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

        // While the session owns the keyboard, show where typed text will
        // land: the current line's cursor cell, in reverse video.
        if let Some(col) = app.monitor_cursor()
            && let Some(text) = app.device_monitor_output.last().map(String::as_str)
            && let Some(last) = console.last_mut()
        {
            *last = cursor_line(text, col);
        }

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

/// Streamed output of a `mpremote run` session, one timestamped line per row.
/// The console always tails the latest output, mirroring the flash output's
/// behavior.
fn draw_run_output(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let script = app
        .run_script
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "script".to_string());

    let status = match app.run_state {
        crate::app::RunState::Idle => "",
        crate::app::RunState::Running => " (running…)",
        crate::app::RunState::Finished => " (done)",
    };

    let title = format!("Run: {script}{status}");
    let block = pane_block(&title, focused);
    let inner_height = block.inner(area).height.max(1) as usize;

    let mut lines: Vec<Line> = app
        .run_output
        .iter()
        .map(|entry| {
            Line::from(vec![
                Span::styled(
                    format!(
                        "{:02}:{:02}:{:02} ",
                        entry.timestamp.hour(),
                        entry.timestamp.minute(),
                        entry.timestamp.second()
                    ),
                    Style::new().dim(),
                ),
                Span::raw(entry.text.clone()),
            ])
        })
        .collect();

    if lines.is_empty() {
        lines.push(Line::from("(no output yet)".dim()));
    }

    let visible_start = lines.len().saturating_sub(inner_height);
    let visible: Vec<Line> = lines.split_off(visible_start);

    frame.render_widget(
        Paragraph::new(visible)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Rebuilds `text` with the cell at byte offset `col` in reverse video ---
/// a stand-in terminal cursor, since the Monitor is a pane, not the real
/// screen. The char under the cursor is highlighted; at end of line a
/// reverse-video blank extends the line by one cell.
fn cursor_line(text: &str, col: usize) -> Line<'static> {
    // `LineConsole` keeps `col` on a char boundary and within the line;
    // `min` only guards a first empty chunk before any text arrived.
    let col = col.min(text.len());
    let under = text[col..].chars().next();
    let rest = col + under.map_or(0, |c| c.len_utf8());

    Line::from(vec![
        Span::raw(text[..col].to_string()),
        match under {
            Some(c) => Span::styled(c.to_string(), Style::new().reversed()),
            None => Span::styled(" ", Style::new().reversed()),
        },
        Span::raw(text[rest..].to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;

    use super::cursor_line;

    #[test]
    fn the_cell_under_the_cursor_is_reversed() {
        let line = cursor_line("abcd", 2);
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].content, "ab");
        assert_eq!(line.spans[1].content, "c");
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(line.spans[2].content, "d");
        // Untouched spans carry no highlight.
        assert!(
            !line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn end_of_line_shows_a_reversed_blank() {
        let line = cursor_line("abc", 3);
        assert_eq!(line.spans[0].content, "abc");
        assert_eq!(line.spans[1].content, " ");
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(line.spans[2].content, "");
    }

    #[test]
    fn multibyte_cells_highlight_whole_chars() {
        // 'é' is two bytes; the cursor must not split it.
        let text = "aé";
        let line = cursor_line(text, 1);
        assert_eq!(line.spans[0].content, "a");
        assert_eq!(line.spans[1].content, "é");
        assert_eq!(line.spans[2].content, "");
    }
}
