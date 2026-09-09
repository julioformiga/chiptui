//! Row 3's Monitor tab: whichever live process output the user last asked
//! for --- a running/just-finished flash (`esptool`) command, a backend
//! build command (`west build`), or a live device serial session --- rendered
//! in one place instead of a separate dialog
//! (`SPEC.md` §11).
//!
//! Build/flash/run feeds remain line documents. The device session is a full
//! VT100 grid, so Zephyr shell cursor movement, colours and redraws survive.
//! Both shapes share `App::monitor_scroll`, hold a scrolled view as output
//! arrives, and resume tail-following at the bottom.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Wrap};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, Focus, MonitorSource, MonitorView};
use crate::backend::Capability;
use crate::flash::{OptionsField, RunState};
use crate::logs::wrap_rows;
use crate::ui::flash::{field_label, field_value};
use crate::ui::{
    Palette, dashboard_behind_dialog, dashboard_focused, draw_scrollbar, muted_style, output_style,
    pane_border,
};

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
            .map(|line| Line::from(line.clone()).fg(palette.fg))
            .collect();
        if doc.is_empty() {
            doc.push(Line::from("(no output yet)".fg(palette.muted)));
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
    // Keep the dormant grid at the pane's real size too. `m` can spawn a
    // chatty monitor before the next frame; sizing only after output arrived
    // would make vt100 shrink its initial 24-row screen and discard its tail.
    let terminal_block = console_block(focused, palette);
    let terminal_layout = console_layout(&terminal_block, area);
    let (_, terminal_rows, terminal_cols) = terminal_layout;
    app.resize_device_monitor(terminal_rows as u16, terminal_cols as u16);

    if app.device_monitor_process.is_some()
        || !app.device_monitor_output.is_empty()
        || !app
            .device_monitor_terminal
            .screen()
            .contents()
            .trim()
            .is_empty()
    {
        let block = terminal_block;
        let layout = terminal_layout;
        let (inner, viewport, width) = layout;

        if app
            .device_monitor_terminal
            .screen()
            .contents()
            .trim()
            .is_empty()
            && app.device_monitor_output.is_empty()
        {
            app.monitor_view = MonitorView {
                rows: 1,
                viewport,
                width,
            };
            frame.render_widget(
                Paragraph::new("(connected)".fg(palette.muted))
                    .block(block)
                    .style(output_style(app)),
                area,
            );
            return;
        }

        let history = app.device_monitor_terminal.scrollback_len();
        app.monitor_view = MonitorView {
            rows: history + viewport,
            viewport,
            width,
        };
        let max = app.monitor_view.rows.saturating_sub(viewport);
        let first = if app.monitor_scroll.following {
            max
        } else {
            app.monitor_scroll.offset.min(max)
        };
        app.device_monitor_terminal
            .set_scrollback(max.saturating_sub(first));

        let cursor = tui_term::widget::Cursor::default().visibility(app.monitor_cursor().is_some());
        frame.render_widget(
            PseudoTerminal::new(app.device_monitor_terminal.screen())
                .block(block)
                .cursor(cursor),
            area,
        );
        draw_scrollbar(
            frame,
            inner,
            app.monitor_view.rows,
            viewport,
            first,
            palette,
        );
        if dashboard_behind_dialog(app) {
            frame
                .buffer_mut()
                .set_style(inner, Style::new().add_modifier(Modifier::DIM));
        }
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
        Paragraph::new(message.fg(palette.muted))
            .block(block)
            .style(output_style(app)),
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
                    Span::styled(format!("{:<10}", field_label(*field)), muted_style(palette)),
                    Span::styled(field_value(flash, *field), Style::new().fg(palette.fg)),
                ])
            })
            .collect();
        if let Some(firmware) = flash.selected_firmware_path() {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<10}", "firmware"), muted_style(palette)),
                Span::styled(firmware.display().to_string(), Style::new().fg(palette.fg)),
            ]));
        }
        lines.push(Line::from(rule.fg(palette.muted)));
        lines.push(Line::from("console".fg(palette.muted)));
        (block, lines)
    } else {
        (console_block(focused, palette), Vec::new())
    };
    let layout = console_layout(&block, area);

    let mut console: Vec<Line> = flash
        .output
        .iter()
        .map(|line| Line::from(line.clone()).fg(palette.fg))
        .collect();
    // The header already reads "(done)"/"(failed)": a banner line here would
    // just be a second, redundant announcement mixed into the command's real
    // output.
    if let RunState::Failed(error) = &flash.state {
        console.push(Line::from(""));
        console.push(Line::from(error.clone().fg(palette.error)));
    }
    if console.is_empty() {
        console.push(Line::from("(no output yet)".fg(palette.muted)));
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
                    muted_style(palette),
                ),
                Span::styled(entry.text.clone(), Style::new().fg(palette.fg)),
            ])
        })
        .collect();

    if lines.is_empty() {
        lines.push(Line::from("(no output yet)".fg(palette.muted)));
    }

    render_console(frame, area, block, layout, &lines, app, palette);
}

/// A console pane's block: like [`pane_border`] (the Log/Monitor tab strip
/// owns the border row), but with the rightmost content column reserved for
/// the scrollbar --- always, so wrapped lines do not reflow the moment the
/// console outgrows the pane (the same rule the Log pane follows).
pub(crate) fn console_block(focused: bool, palette: Palette) -> Block<'static> {
    pane_border(focused, palette).padding(Padding::right(1))
}

/// The pane geometry every console renderer needs: the inner area *with* the
/// reserved gutter included (where the scrollbar draws), the row budget, and
/// the wrap width (the padded width the `Paragraph` wraps at --- the row
/// counter must agree with it).
type ConsoleLayout = (Rect, usize, usize);

pub(crate) fn console_layout(block: &Block<'_>, area: Rect) -> ConsoleLayout {
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
pub(crate) fn render_console(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    layout: ConsoleLayout,
    doc: &[Line<'_>],
    app: &mut App,
    palette: Palette,
) {
    let (inner, viewport, width) = layout;
    // Every console lives in row 3: focus is Logs' alone, derived rather
    // than threaded through as another argument.
    let focused = dashboard_focused(app, Focus::Logs);
    // Read before the mutable borrow below: the console dims behind a
    // dialog like every other pane, but never merely because the cursor
    // sits elsewhere (see [`crate::ui::output_style`]).
    let style = output_style(app);
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
    // The focused tint under the console's text (fg-only styles), borders
    // excluded --- every pane's interior, same rule.
    super::paint_focus_wash(frame, inner, focused, palette);
    frame.render_widget(
        Paragraph::new(visible)
            .block(block)
            .style(style)
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

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::{plain, window_console};

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
}
