//! Row 3's Terminal tab: the user's own shell, streamed from a PTY session
//! (`src/app/terminal.rs` owns the session; this is only its rendering).
//!
//! Unlike the Monitor tab, this is not a document of lines: it is a cell
//! grid, drawn by `tui-term` straight out of the `vt100` emulator, so the
//! shell's own colours and attributes reach the screen. The pane's palette
//! supplies only the *default* foreground and background --- the colour of a
//! cell the shell never styled. Overriding the rest would defeat the point.
//!
//! The pane also owns the geometry: the inner rect it measures here is what
//! the emulator and the child are resized to, which is how a prompt that
//! places its right-hand segment by column lands where it means to.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::widgets::Paragraph;
use tui_term::widget::PseudoTerminal;

use crate::app::{App, Focus};
use crate::ui::monitor::console_block;
use crate::ui::{
    Palette, dashboard_behind_dialog, dashboard_focused, output_style, paint_focus_wash,
    pane_border,
};

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    let focused = dashboard_focused(app, Focus::Logs);

    if app.terminal_process.is_none() && app.terminal.screen().contents().trim().is_empty() {
        // No session and no transcript: the tab starts its shell on entry,
        // so this is the spawn-failed state --- the reason is in the log
        // pane, and `r` starts another without leaving the tab.
        let block = pane_border(focused, palette);
        paint_focus_wash(frame, block.inner(area), focused, palette);
        frame.render_widget(
            Paragraph::new("shell not running — press r to start one".fg(palette.muted))
                .block(block)
                .style(output_style(app)),
            area,
        );
        return;
    }

    let block = console_block(focused, palette);
    let inner = block.inner(area);
    // The emulator and the child both follow the pane. `resize_terminal` is
    // a no-op unless the size actually changed, so calling it every frame
    // costs nothing and never fires a spurious SIGWINCH.
    app.resize_terminal(inner.height.max(1), inner.width.max(1));

    // Scrolling the tab scrolls the grid's own history: `monitor_scroll`
    // counts rows from the tail, which is exactly what `set_scrollback`
    // takes. Publishing the geometry keeps the key handlers' clamp and the
    // tab strip's `↑N` honest, the same contract `render_console` has.
    let rows = app.terminal.scrollback_len();
    let viewport = inner.height.max(1) as usize;
    app.monitor_view = crate::app::MonitorView {
        rows: rows + viewport,
        viewport,
        width: inner.width.max(1) as usize,
    };
    let back = if app.monitor_scroll.following {
        0
    } else {
        let max = app.monitor_view.rows.saturating_sub(viewport);
        max.saturating_sub(app.monitor_scroll.offset.min(max))
    };
    app.terminal.set_scrollback(back);

    // The cursor is placed by the widget from the grid's own position; all
    // this decides is whether it shows. The real terminal cursor stays
    // hidden (`terminal::init` issues `Hide`), so this is the only one the
    // user sees --- and it belongs to the shell only while the shell has
    // the keyboard (`App::terminal_cursor` also honours the child hiding
    // it, which every full-screen program does while redrawing).
    let cursor = tui_term::widget::Cursor::default().visibility(app.terminal_cursor().is_some());
    // No `.style()`: `PseudoTerminal` ignores it (it writes `Style::reset()`
    // per cell), and it would be the wrong thing anyway. Every cell carries
    // the shell's own colour, and a cell the shell never styled maps to
    // `Color::Reset` --- the host terminal's default, which is exactly what
    // the same shell shows outside ChipTUI. Substituting `palette.fg` here
    // would be the bug, not the feature. For the same reason the focused
    // pane's tint is not painted under the grid: the emulator resets every
    // inner cell's background, so the wash could never survive it --- the
    // border accent is this tab's focus indicator while a shell lives.
    frame.render_widget(
        PseudoTerminal::new(app.terminal.screen())
            .block(block)
            .cursor(cursor),
        area,
    );

    // The dimming every output pane does behind a dialog
    // (`ui::output_style`) cannot ride the widget for the same reason, so it
    // is patched over the drawn cells --- the technique `ui::button` uses
    // for a selected row.
    if dashboard_behind_dialog(app) {
        frame
            .buffer_mut()
            .set_style(inner, Style::new().add_modifier(Modifier::DIM));
    }
}
