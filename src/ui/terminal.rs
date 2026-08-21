//! Row 3's Terminal tab: the user's own shell, streamed from a PTY session
//! (`src/app/terminal.rs` owns the session; this is only its rendering).
//!
//! The pane is the same console the Monitor tab draws, scroll included
//! (while the shell does not own the keyboard --- attached, every arrow key
//! goes into the PTY instead), with the reverse-video cursor cell showing
//! where typed text will land.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus};
use crate::ui::monitor::{console_block, console_layout, cursor_line, render_console};
use crate::ui::{Palette, dashboard_focused, output_style, pane_border};

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    let focused = dashboard_focused(app, Focus::Logs);

    if app.terminal_process.is_some() || !app.terminal_output.is_empty() {
        let block = console_block(focused, palette);
        let layout = console_layout(&block, area);

        let mut console: Vec<Line> = app
            .terminal_output
            .iter()
            .map(|line| Line::from(line.clone()).fg(palette.fg))
            .collect();

        // While the shell owns the keyboard, show where typed text will
        // land: the current line's cursor cell, in reverse video --- the
        // same stand-in the device monitor's console draws.
        if let Some(col) = app.terminal_cursor()
            && let Some(text) = app.terminal_output.last().map(String::as_str)
            && let Some(last) = console.last_mut()
        {
            *last = cursor_line(text, col);
        }

        if console.is_empty() {
            console.push(Line::from("(shell starting)".fg(palette.muted)));
        }

        render_console(frame, area, block, layout, &console, app, palette);
        return;
    }

    // No session and no transcript: the tab starts its shell on entry, so
    // this is the spawn-failed state --- the reason is in the log pane.
    let block = pane_border(focused, palette);
    frame.render_widget(
        Paragraph::new(
            "shell not running — switch tabs away and back to start one".fg(palette.muted),
        )
        .block(block)
        .style(output_style(app)),
        area,
    );
}
