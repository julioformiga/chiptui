//! Rendering.
//!
//! Rendering is a pure function of [`App`] state (plus the log viewport height,
//! which the renderer publishes back so scrolling matches what is on screen).
//! Colors come from [`App::theme_palette`] --- a `ratatui-themes` palette
//! (default: Tokyo Night, overridable via `[ui] theme` in the user config,
//! see `settings.rs`) computed once per frame in [`draw`] and threaded down
//! through every `draw_*` call as a `Palette` parameter, the same way `Focus`
//! already is.

mod build;
mod button;
mod files;
mod flash;
pub mod home;
mod monitor;
mod overlay;
mod panels;
mod workspace;

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::app::{App, Focus, LogTab, View};
use crate::backend::BackendKind;

/// Below this the panes cannot be rendered legibly.
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 14;

/// Frames of the shared "something is running" spinner, animated off
/// [`App::ticks`] (one frame per tick). Used by the file panes' waits, the
/// board picker's fetch, and the Monitor tab's live status.
pub(crate) const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

/// The active theme, threaded through render calls the same way `Focus`/
/// `bool` already are --- computed once per frame in [`draw`] from
/// [`App::theme_palette`] and passed down, rather than re-read off `App` at
/// every call site (a single source of truth for the frame being drawn).
pub(crate) type Palette = ratatui_themes::ThemePalette;

/// The theme's selection colors: `selection` as a background under `fg`
/// text --- the explicit, deterministic fill [`crate::ui::button`] already
/// uses, shared by every list and row highlight so a selected row reads the
/// same wherever the cursor lands. Not `Modifier::REVERSED`, which swaps
/// the terminal's own defaults and ignores the active theme entirely.
pub(crate) fn selection_style(palette: Palette) -> Style {
    Style::new().fg(palette.fg).bg(palette.selection)
}

/// The theme's muted color --- dimmed text, placeholders, labels, legends
/// and other secondary information (`palette.muted`'s documented role).
/// Used instead of a bare `Modifier::DIM`, which keeps the terminal's
/// default foreground and therefore ignores the active theme.
pub(crate) fn muted_style(palette: Palette) -> Style {
    Style::new().fg(palette.muted)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        return;
    }

    let palette = app.theme_palette();

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, header, app, palette);

    // The flash view is a dialog layered over the dashboard, never a
    // full-screen replacement, so the project/device/files panes stay
    // visible (and, via `pane`'s `View::Dashboard` check below, visibly
    // dimmed) while esptool commands run.
    draw_dashboard(frame, body, app, palette);
    if app.view == View::Flash {
        draw_flash_dialog(frame, body, app, palette);
    }

    draw_footer(frame, footer, app, palette);
    overlay::draw(frame, area, app, palette);
}

/// The flash action menu/options, sized to its content (like
/// `overlay::draw_confirm`/`draw_device_picker`) and centered over `body`
/// with a `Clear` behind it so the dashboard shows through around the edges
/// --- a real dialog, not a near-fullscreen replacement. Running an action
/// closes this dialog (`App::show_flash_in_monitor`), so it never needs to
/// size itself for streamed output.
fn draw_flash_dialog(frame: &mut Frame, body: Rect, app: &App, palette: Palette) {
    let Some(flash) = &app.flash else { return };
    let (width, height) = flash::dialog_size(flash);
    let popup = centered(body, width, height);
    frame.render_widget(Clear, popup);
    flash::draw(frame, popup, app, palette);
}

/// Centers a `width`×`height` box inside `area`, shrinking to fit. Shared
/// with `overlay::draw` --- every modal in this app sizes itself off its own
/// content rather than a fraction of the screen.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    popup
}

/// Row 1: Project | Device, split evenly. Row 2: the dual-pane file browser
/// when the backend has `Capability::Filesystem`, else a full-width
/// placeholder. Row 3: one bordered pane whose top border carries the
/// Log/Monitor tab strip over the selected tab's body, full width
/// (`SPEC.md` §11).
///
/// Row 1 is a fixed height: both info panes pad their content to
/// [`panels::INFO_ROWS`] lines in every backend and state, so the rows below
/// never shift when a workspace resolves or device details accumulate. Both
/// are informational (never focused).
fn draw_dashboard(frame: &mut Frame, body: Rect, app: &mut App, palette: Palette) {
    // The info panes' content rows plus their borders.
    let info_height = panels::INFO_ROWS as u16 + 2;

    let [row1, rest] =
        Layout::vertical([Constraint::Length(info_height), Constraint::Min(0)]).areas(body);
    // Row 2 leans on its content when the workspace/project panes claim
    // it: their stacked button groups (checklist rows, a separator, a rule
    // per button edge, the pinned state line) are the tallest content on
    // the dashboard, so the row is sized to fit them and the log pane
    // (which scrolls) takes the remainder. The browser keeps the
    // historical 60/40 split.
    let [row2, row3] = if app.workspace_pane_visible() {
        let needed = row2_content_height(app)
            .saturating_add(2) // the pane's borders (the state line is content, already counted)
            .min(rest.height.saturating_sub(3).max(1));
        Layout::vertical([Constraint::Length(needed), Constraint::Min(0)]).areas(rest)
    } else {
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(rest)
    };
    let [project, device] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(row1);

    panels::draw_project(frame, project, app, palette);
    panels::draw_detection(frame, device, app, palette);

    // Row 2 belongs to whichever panes the backend's capabilities give it:
    // the dual-pane file browser under `Capability::Filesystem`, the
    // workspace+build pair for a backend that builds without a device
    // filesystem (`SPEC.md` §11), and a placeholder only in the window
    // before the panes exist at all.
    if app.workspace_pane_visible() {
        workspace::draw_row(frame, row2, app, palette);
    } else if app.browser.is_some() {
        files::draw(frame, row2, app, palette);
    } else {
        panels::draw_no_filesystem(frame, row2, app, palette);
    }

    // Row 3 is one bordered pane for the whole width: the Log/Monitor tab
    // strip lives on the pane's own top border (`SPEC.md` §11), like the
    // Ratatui `Tabs` example, and the selected tab's body fills the pane.
    match app.log_tab {
        LogTab::Log => panels::draw_logs(frame, row3, app, palette),
        LogTab::Monitor => monitor::draw(frame, row3, app, palette),
    }
    panels::draw_log_tabs(frame, row3, app, palette);
}

/// The minimum rows the workspace pane's embedded file list gets, past the
/// checklist and its header line --- enough to show a handful of entries
/// without scrolling. Row 2 is sized to fit exactly this much (like the
/// checklist rows above it); a taller terminal gives the remainder to row 3
/// (the log/monitor pane), the same trade-off row 2 already makes today.
const MIN_FILES_ROWS: u16 = 6;

/// The taller row-2 pane's inner content height: the workspace pane's
/// checklist rows (plus an invalid location's wrapped reason), a separator,
/// the file-list header and its minimum rows on one side; the project
/// pane's stacked button group --- one row per button, one rule at each
/// edge and one divider between each pair --- on the other.
fn row2_content_height(app: &App) -> u16 {
    let caps = app.manager.capabilities();
    let workspace = app.workspace.as_ref().map_or(0, |panel| {
        let checklist = panel.actions(&caps).len() as u16;
        let invalid = if panel.invalid.is_some() { 4 } else { 0 };
        let separator = 1;
        let files_header = 1;
        checklist + invalid + separator + files_header + MIN_FILES_ROWS
    });
    let build = app.build.as_ref().map_or(0, |panel| {
        // The stacked group plus a three-row footer, reserved whether or
        // not the `Stop` box is showing (`Stop` is appended to the list,
        // never a stacked row, so the group itself never changes size):
        // the pane's height must not change when a command starts.
        let mains = panel.actions(&caps).len() - usize::from(panel.is_busy());
        (2 * mains + 1 + 3) as u16
    });
    workspace.max(build)
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    let message = Paragraph::new(vec![
        Line::from("Terminal too small".bold()),
        Line::from(format!(
            "need at least {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
            area.width, area.height
        )),
    ])
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: true });
    frame.render_widget(message, area);
}

/// The title bar: the badge and the backend on the left, the project
/// centered, the device on the right.
///
/// The center is centered on the whole bar but clamped into the space the
/// two sides leave free, so the three zones never overwrite each other; a
/// name too long for that space ellipsizes, and the section drops only
/// when not even `Project …` fits. The right side --- the volatile half
/// (a board plugs in, a scan finishes) --- never truncates.
fn draw_header(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let left = backend_spans(app, palette);
    let right = device_status(app, palette);
    let left_width = spans_width(&left);
    let right_width = spans_width(&right);

    frame.render_widget(Paragraph::new(Line::from(left)), area);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        area,
    );

    let zone_start = left_width + 1;
    let zone = (area.width as usize)
        .saturating_sub(right_width + 1)
        .saturating_sub(zone_start);
    let Some(center) = project_spans(app, palette, zone) else {
        return;
    };
    let center_width = spans_width(&center) as u16;
    let x = (area.width.saturating_sub(center_width) / 2)
        .max(zone_start as u16)
        .min(area.width.saturating_sub(right_width as u16 + center_width));
    frame.render_widget(
        Paragraph::new(Line::from(center)),
        Rect {
            x,
            y: area.y,
            width: center_width,
            height: 1,
        },
    );
}

/// The header's left side: the badge, then the backend section. Labels
/// carry the muted color and values the weight (bold default), so what the
/// user asked about reads louder than the word that marked it.
fn backend_spans(app: &App, palette: Palette) -> Vec<Span<'static>> {
    let (icon, backend) = match app.manager.selected_kind() {
        Some(BackendKind::Zephyr) => ("◆", BackendKind::Zephyr.display_name()),
        Some(BackendKind::MicroPython) => ("▲", BackendKind::MicroPython.display_name()),
        None => ("◇", "none"),
    };
    vec![
        Span::styled(
            " ChipTUI ",
            Style::new().fg(palette.bg).bg(palette.accent).bold(),
        ),
        Span::raw(" "),
        Span::styled("Backend", Style::new().fg(palette.muted)),
        Span::raw(" "),
        Span::styled(icon, Style::new().fg(palette.accent)),
        Span::raw(" "),
        Span::styled(backend, Style::new().fg(palette.fg).bold()),
    ]
}

/// The header's center: the project question's answer. `None` while the
/// question is unanswered, or when the free zone cannot hold even
/// `Project …` --- a lone label or a dangling cut is noise.
fn project_spans(app: &App, palette: Palette, zone: usize) -> Option<Vec<Span<'static>>> {
    let project = app.header_project();
    if project.is_empty() || zone < "Project …".len() {
        return None;
    }
    let keep = zone - "Project ".len();
    let name = if project.chars().count() <= keep {
        project
    } else {
        let mut short: String = project.chars().take(keep - 1).collect();
        short.push('…');
        short
    };
    Some(vec![
        Span::styled("Project", Style::new().fg(palette.muted)),
        Span::raw(" "),
        Span::styled(name, Style::new().fg(palette.fg).bold()),
    ])
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.width()).sum()
}

/// The header's right edge: the connection icon plus the port when a
/// device answers, dim and with the reason when none does.
fn device_status(app: &App, palette: Palette) -> Vec<Span<'static>> {
    match app.devices.selected() {
        Some(device) => vec![
            Span::styled("● ", Style::new().fg(palette.success)),
            Span::styled(device.port.clone(), Style::new().fg(palette.fg)),
        ],
        None => vec![
            Span::styled("○ ", muted_style(palette)),
            Span::styled(app.devices.header_status(), muted_style(palette)),
        ],
    }
}

/// The contextual shortcut line.
///
/// More hints than columns is normal on a narrow terminal, and the two that
/// must survive are the last ones (`?` help, `q` quit --- the way out and
/// the way to the rest). So hints are dropped whole, from the *middle*,
/// rather than letting the line truncate mid-word: a cut-off " q  qui" is
/// worse than one fewer hint.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    const KEEP_LAST: usize = 2;

    let mut hints = app.shortcuts();
    let width = |hints: &[(&str, &str)]| -> usize {
        hints
            .iter()
            .map(|(key, label)| key.chars().count() + label.chars().count() + 5)
            .sum()
    };
    while width(&hints) > area.width as usize && hints.len() > KEEP_LAST {
        hints.remove(hints.len() - KEEP_LAST - 1);
    }

    let mut spans = Vec::new();
    for (key, label) in hints {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(palette.fg).bg(palette.muted),
        ));
        spans.push(Span::styled(format!(" {label}  "), muted_style(palette)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Whether `focus` currently has the user's attention on the dashboard ---
/// false whenever the flash dialog or another overlay has it instead, in
/// which case both a pane's border (`pane_block`) and its content
/// (`content_style`) should read as dimmed.
fn dashboard_focused(app: &App, focus: Focus) -> bool {
    app.view == View::Dashboard && app.focus == focus && app.overlay.is_none()
}

/// Style for a dashboard pane's content: unchanged when focused, dimmed
/// otherwise. Ratatui has no dedicated "darken the background" primitive ---
/// a terminal buffer has no compositing/alpha to blend against, so this is
/// the closest equivalent: `Modifier::DIM` set at the widget level, which
/// cascades onto every already-colored `Span` drawn inside instead of
/// requiring each one to know about focus.
fn content_style(focused: bool) -> Style {
    if focused {
        Style::default()
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

/// Renders `path` for display, collapsing a `home` prefix to `~` (the form
/// the user types and reads, and shorter in a pane's answer column). A path
/// *next to* home (`/home/julio-dev` beside `/home/julio`) keeps its full
/// form: `Path::strip_prefix` compares whole components, never bytes.
pub(crate) fn tilde_path(path: &Path, home: &Path) -> String {
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// A bordered block that shows whether it holds focus.
fn pane_block(title: &str, focused: bool, palette: Palette) -> Block<'static> {
    pane_border(focused, palette).title(title_span(title, focused, palette))
}

/// An untitled bordered block that shows whether it holds focus: row 3's
/// pane, whose border row belongs to the Log/Monitor tab strip (with the
/// active tab's status at its right --- see `panels::draw_log_tabs`).
pub(crate) fn pane_border(focused: bool, palette: Palette) -> Block<'static> {
    let border = if focused {
        Style::new().fg(palette.accent)
    } else {
        Style::new().fg(palette.muted)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
}

fn title_span(title: &str, focused: bool, palette: Palette) -> Line<'static> {
    let title_style = if focused {
        Style::new().fg(palette.accent).add_modifier(Modifier::BOLD)
    } else {
        muted_style(palette)
    };
    Line::from(Span::styled(format!(" {title} "), title_style))
}

/// The discreet one-column scrollbar shared by the Log and Monitor panes:
/// thin dim track, slightly brighter thumb, no arrow heads. Drawn just inside
/// `inner`'s right edge --- callers reserve that column (the Log pane shrinks
/// its wrap width; the Monitor pane pads its block) so wrapped text never
/// reflows when the bar appears. `content` and `viewport` are visual
/// (post-wrap) rows; `position` is the first visible row.
pub(crate) fn draw_scrollbar(
    frame: &mut Frame,
    inner: Rect,
    content: usize,
    viewport: usize,
    position: usize,
    palette: Palette,
) {
    if content <= viewport || inner.width == 0 || inner.height == 0 {
        return;
    }
    let top = position.min(content - viewport);
    let mut state = ScrollbarState::new(content)
        .viewport_content_length(viewport)
        .position(top);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .thumb_symbol("┃")
        .track_style(Style::new().fg(palette.muted))
        .thumb_style(Style::new().fg(palette.fg));
    frame.render_stateful_widget(
        bar,
        Rect {
            x: inner.right() - 1,
            y: inner.y,
            width: 1,
            height: inner.height,
        },
        &mut state,
    );
}

#[cfg(test)]
mod tests {
    use super::tilde_path;
    use std::path::Path;

    #[test]
    fn home_prefixed_paths_collapse_to_a_tilde() {
        let home = Path::new("/home/julio");
        assert_eq!(
            tilde_path(Path::new("/home/julio/zephyrproject"), home),
            "~/zephyrproject"
        );
        assert_eq!(tilde_path(Path::new("/home/julio"), home), "~");
        // Component-wise, never byte-wise: a sibling directory that merely
        // shares the prefix keeps its full form.
        assert_eq!(
            tilde_path(Path::new("/home/julio-dev/app"), home),
            "/home/julio-dev/app"
        );
        assert_eq!(tilde_path(Path::new("/opt/app"), home), "/opt/app");
        // An unset home (empty) must not claim every path as its own.
        assert_eq!(tilde_path(Path::new("/opt/app"), Path::new("")), "/opt/app");
    }
}
