//! Row 3's Monitor tab: whichever live process output the user last asked
//! for --- a running/just-finished flash (`esptool`) command, a backend
//! build command (`west build`), or (once wired up) a live device serial
//! session --- rendered in one place instead of a separate dialog
//! (`SPEC.md` §11).
//!
//! Every console scrolls like the Log pane (`↑/↓`, `PageUp/Down`, `Home`/
//! `End`), counted in wrapped rows. The scroll anchors to the top of the
//! document (`App::monitor_scroll`), so live output arriving while the user
//! is scrolled back grows the document *below* the view and never shifts it;
//! scrolling back to the bottom resumes tail-following.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Wrap};

use crate::app::{App, Focus, MonitorSource, MonitorView};
use crate::backend::Capability;
use crate::flash::{OptionsField, RunState};
use crate::logs::wrap_rows;
use crate::ui::flash::{field_label, field_value};
use crate::ui::{Palette, content_style, dashboard_focused, draw_scrollbar, pane_border};

/// Chip + offset, always meaningful for `WriteFlash`/`VerifyFlash` on the
/// recap above the console --- the options screen's full field list
/// (`FlashPanel::options_fields`) also covers flash mode/freq/size/extra
/// flags, more detail than a short recap needs.
const RECAP_FIELDS: &[OptionsField] = &[OptionsField::Chip, OptionsField::Offset];

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    let focused = dashboard_focused(app, Focus::Logs);
    match app.monitor_source {
        MonitorSource::Flash => draw_flash_output(frame, area, app, focused, palette),
        MonitorSource::Device => draw_device_monitor(frame, area, app, focused, palette),
        MonitorSource::Run => draw_run_output(frame, area, app, focused, palette),
        MonitorSource::Build => draw_build_output(frame, area, app, focused, palette),
    }
}

/// Streamed build-command output (`west build`), mirroring the flash
/// output's shape: the panel's own header carries board/state, and the tab
/// strip carries the live status (see `panels::monitor_status`), so this
/// pane is just the console.
fn draw_build_output(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    focused: bool,
    palette: Palette,
) {
    let doc: Vec<Line> = {
        let Some(panel) = app.build.as_ref() else {
            draw_device_monitor(frame, area, app, focused, palette);
            return;
        };

        let mut doc: Vec<Line> = panel
            .output
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect();
        if doc.is_empty() {
            doc.push(Line::from("(no output yet)".dim()));
        }
        doc
    };

    let block = console_block(focused, palette);
    let layout = console_layout(&block, area);
    render_console(frame, area, block, layout, &doc, app, palette);
}

/// The live device serial/REPL session, or its placeholders.
fn draw_device_monitor(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    focused: bool,
    palette: Palette,
) {
    if app.device_monitor_process.is_some() || !app.device_monitor_output.is_empty() {
        let block = console_block(focused, palette);
        let layout = console_layout(&block, area);

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

        render_console(frame, area, block, layout, &console, app, palette);
        return;
    }

    let block = pane_border(focused, palette);

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
fn draw_flash_output(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    focused: bool,
    palette: Palette,
) {
    let Some(flash) = app.flash.as_ref() else {
        draw_device_monitor(frame, area, app, focused, palette);
        return;
    };

    let action = flash.selected_action();

    // `WriteFlash`/`VerifyFlash` get a short read-only recap of what is
    // actually running above the console --- `SPEC.md` §15's "never hide
    // what is running behind a paraphrase" applies here too, not just to the
    // confirmation overlay. Other actions have nothing to recap, so the
    // console gets the whole pane. The recap scrolls with the console (one
    // document), so the scrollbar stays honest about where the view is; the
    // live status icon and title ride on the tab strip instead
    // (`panels::monitor_status`).
    let (block, mut doc) = if action.needs_firmware() {
        let block = console_block(focused, palette);
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
        lines.push(Line::from("console".dim()));
        (block, lines)
    } else {
        (console_block(focused, palette), Vec::new())
    };
    let layout = console_layout(&block, area);

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
        console.push(Line::from(error.clone().fg(palette.error)));
    }
    if console.is_empty() {
        console.push(Line::from("(no output yet)".dim()));
    }
    doc.extend(console);

    render_console(frame, area, block, layout, &doc, app, palette);
}

/// Streamed output of a `mpremote run` session, one timestamped line per row.
fn draw_run_output(frame: &mut Frame, area: Rect, app: &mut App, focused: bool, palette: Palette) {
    let block = console_block(focused, palette);
    let layout = console_layout(&block, area);

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

    render_console(frame, area, block, layout, &lines, app, palette);
}

/// A console pane's block: like [`pane_border`] (the Log/Monitor tab strip
/// owns the border row), but with the rightmost content column reserved for
/// the scrollbar --- always, so wrapped lines do not reflow the moment the
/// console outgrows the pane (the same rule the Log pane follows).
fn console_block(focused: bool, palette: Palette) -> Block<'static> {
    pane_border(focused, palette).padding(Padding::right(1))
}

/// The pane geometry every console renderer needs: the inner area *with* the
/// reserved gutter included (where the scrollbar draws), the row budget, and
/// the wrap width (the padded width the `Paragraph` wraps at --- the row
/// counter must agree with it).
type ConsoleLayout = (Rect, usize, usize);

fn console_layout(block: &Block<'_>, area: Rect) -> ConsoleLayout {
    let inner = block.inner(area);
    let budget = inner.height.max(1) as usize;
    let width = inner.width.max(1) as usize;
    // `inner` already excludes the padded gutter; extend it back over the
    // reserved column, which is where the scrollbar draws.
    let with_gutter = Rect {
        width: inner.width.saturating_add(1),
        ..inner
    };
    (with_gutter, budget, width)
}

/// A `Line`'s content as plain text, for wrap counting.
fn plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.clone().into_owned())
        .collect()
}

/// Renders a console document under the Monitor tab's shared scroll rules:
/// publishes the pane's geometry (so `App`'s key handlers clamp to what is
/// on screen), resolves the visible window from [`App::monitor_scroll`]
/// (tail while following, top-anchored otherwise), draws the paragraph, and
/// the scrollbar at the window's position. Every console goes through here,
/// which is what makes them all scroll identically.
fn render_console(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    layout: ConsoleLayout,
    doc: &[Line<'_>],
    app: &mut App,
    palette: Palette,
) {
    let (inner, viewport, width) = layout;
    let rows: usize = doc.iter().map(|line| wrap_rows(&plain(line), width)).sum();
    app.monitor_view = MonitorView {
        rows,
        viewport,
        width,
    };
    let max_offset = rows.saturating_sub(viewport);
    let first = if app.monitor_scroll.following {
        max_offset
    } else {
        app.monitor_scroll.offset.min(max_offset)
    };

    let (visible, _) = window_console(doc, width, first, viewport);
    frame.render_widget(
        Paragraph::new(visible)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
    draw_scrollbar(frame, inner, rows, viewport, first, palette);
}

/// The lines whose wrapped rows intersect `[first, first + viewport)`, plus
/// the total wrapped rows of the whole document: the Monitor counterpart of
/// the Log pane's `visible_rows`. A line is included whole when any of its
/// rows is visible (the `Paragraph` then clips, as before); `first` past the
/// end yields nothing.
fn window_console<'a>(
    lines: &'a [Line<'_>],
    width: usize,
    first: usize,
    viewport: usize,
) -> (Vec<Line<'a>>, usize) {
    let end = first.saturating_add(viewport.max(1));
    let mut visible = Vec::new();
    let mut cursor = 0;
    for line in lines {
        let rows = wrap_rows(&plain(line), width);
        let next = cursor + rows;
        if next > first && cursor < end {
            visible.push(line.clone());
        }
        cursor = next;
    }
    (visible, cursor)
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
    use ratatui::text::Line;

    use super::{cursor_line, plain, window_console};

    fn lines(texts: &[&str]) -> Vec<Line<'static>> {
        texts.iter().map(|t| Line::from((*t).to_string())).collect()
    }

    fn texts(rendered: &[Line<'_>]) -> Vec<String> {
        rendered.iter().map(plain).collect()
    }

    #[test]
    fn a_console_that_fits_is_returned_whole() {
        let console = lines(&["one", "two"]);
        let (visible, total) = window_console(&console, 40, 0, 10);
        assert_eq!(texts(&visible), vec!["one", "two"]);
        assert_eq!(total, 2);
    }

    #[test]
    fn the_tail_keeps_the_newest_lines_within_the_row_budget() {
        let console = lines(&["a", "b", "c", "d"]);
        let (visible, total) = window_console(&console, 40, 2, 2);
        assert_eq!(texts(&visible), vec!["c", "d"]);
        assert_eq!(total, 4, "the scrollbar still sees everything");
    }

    #[test]
    fn the_window_counts_wrapped_rows_not_lines() {
        // At width 4 each long line is two rows, so one viewport of two rows
        // holds exactly one of them.
        let console = lines(&["aa bb", "cc dd"]);
        let (visible, total) = window_console(&console, 4, 2, 2);
        assert_eq!(texts(&visible), vec!["cc dd"]);
        assert_eq!(total, 4);
    }

    #[test]
    fn a_line_partially_visible_at_the_top_is_included_whole() {
        // `first` cuts into "aa bb"'s second row; both it and the next line
        // intersect the window.
        let console = lines(&["aa bb", "cc"]);
        let (visible, total) = window_console(&console, 4, 1, 2);
        assert_eq!(texts(&visible), vec!["aa bb", "cc"]);
        assert_eq!(total, 3);
    }

    #[test]
    fn a_single_line_taller_than_the_pane_is_still_shown() {
        let console = lines(&["aa bb cc dd ee"]);
        let (visible, total) = window_console(&console, 4, 0, 2);
        assert_eq!(texts(&visible), vec!["aa bb cc dd ee"]);
        assert!(total > 2);
    }

    #[test]
    fn a_window_past_the_end_is_empty() {
        let console = lines(&["a", "b"]);
        let (visible, _) = window_console(&console, 40, 5, 2);
        assert!(visible.is_empty());
    }

    #[test]
    fn an_empty_console_is_empty() {
        let (visible, total) = window_console(&[], 40, 0, 5);
        assert!(visible.is_empty());
        assert_eq!(total, 0);
    }

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
