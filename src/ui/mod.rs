//! Rendering.
//!
//! Rendering is a pure function of [`App`] state (plus the log viewport height,
//! which the renderer publishes back so scrolling matches what is on screen).
//! Colors come from [`App::theme_palette`] --- a `ratatui-themes` palette
//! (default: Tokyo Night, overridable via `[ui] theme` in the user config,
//! see `settings.rs`) computed once per frame in [`draw`] and threaded down
//! through every `draw_*` call as a `Palette` parameter, the same way `Focus`
//! already is. Button glyphs come from [`App::icon_set`] (`[ui] icons`,
//! default plain Unicode) the same way, read off `App` by the calls that
//! build a button stack.

mod build;
mod build_dashboard;
mod button;
pub(crate) use button::{Button, STOP_BOX_WIDTH, button_at_row, stack_height};
mod files;
mod flash;
pub(crate) use flash::dialog_size as flash_dialog_size;
pub mod home;
mod install;
pub(crate) use install::area as install_area;
pub(crate) mod layout;
mod monitor;
mod overlay;
pub(crate) use overlay::ZEPHYR_ACTIONS_COUNT;
mod panels;
pub(crate) use panels::board_shield_click_is_board;
pub(crate) use panels::device_mac_row;
mod terminal;
mod workspace;

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::app::{App, Focus, LogTab, View};
use crate::backend::BackendKind;

/// Below this the dashboard cannot be rendered legibly --- and the number
/// is the *measured* one, not an aspiration: at 80x32 the row-2 button
/// stack (six actions: one rule per edge, a divider between each pair, and
/// the always-reserved three-row footer = 18 rows with its borders) fits
/// whole, and row 3 still gets four content rows of log. One row less and
/// the stack loses its bottom rule; one column less and the Device info
/// pane's chip line wraps, pushing the `Firmware` row out of its fixed
/// four. Anything that grows row 2 has to move these numbers with it.
const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 32;

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

/// A decoration mark padded into the fixed three-cell icon column: two
/// cells for the glyph, one separating space. Every emoji mark draws two
/// cells wide in any terminal, but some marks --- the Nerd set's Private
/// Use Area glyphs, and the plain `◆` diamond [`crate::icons::IconSet::
/// zephyr`] shares with the header --- are single-width, and a width-1
/// glyph needs two pads, not one: a trailing pad alone keeps the text
/// columns lined up but parks the glyph at the column's left edge, a
/// full cell left of a two-cell mark's visual center, so the *marks*
/// read misaligned against each other. The leading pad centers a
/// single-cell glyph over the second cell of a two-cell mark's span ---
/// the closest a one-cell glyph gets to the middle of one two cells
/// wide. `single_cell` is the caller's own knowledge of its glyph's
/// width, not a guess from the codepoint --- a mark drawn single-width
/// is never inferable from its range alone (`◆` sits outside the PUA
/// same as any emoji, yet draws one cell). Both columns that budget the
/// three cells (the home screen's project rows, the file panes' kind
/// column) draw through here, so the pad is stated once.
fn icon_column(mark: &str, single_cell: bool) -> String {
    let lead = if single_cell { " " } else { "" };
    format!("{lead}{mark} ")
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // Published before anything else: the mouse hit-testing recomputes the
    // layout from this area, and a frame that cannot draw the dashboard
    // leaves no geometry to click on (`None`, not a stale rect).
    app.frame_area = if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        None
    } else {
        Some(area)
    };
    if app.frame_area.is_none() {
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
/// content rather than a fraction of the screen --- and with the mouse
/// hit-testing, which re-derives a dialog's button rects through the same
/// call so a click lands on exactly the box that was drawn.
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
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
/// The geometry itself lives in [`layout::dashboard`] --- one tree shared
/// with the mouse hit-testing, so a click lands on exactly the rect the
/// frame drew. This function only dispatches each pane's renderer onto
/// its rect.
///
/// Row 1 is a fixed height: both info panes pad their content to
/// [`panels::INFO_ROWS`] lines in every backend and state, so the rows below
/// never shift when a workspace resolves or device details accumulate. Both
/// are informational (never focused).
fn draw_dashboard(frame: &mut Frame, body: Rect, app: &mut App, palette: Palette) {
    let areas = layout::dashboard(app, body);

    // `ctrl+f`: row 3 alone, full body --- panes 1/2 are undrawn rather
    // than drawn into the zero-size rects `layout::dashboard` handed back
    // for them, which would be wasted work at best.
    if !app.row3_fullscreen {
        panels::draw_project(frame, areas.project, app, palette);
        panels::draw_detection(frame, areas.device, app, palette);

        // Row 2 belongs to whichever panes the backend's capabilities give it:
        // the dual-pane file browser under `Capability::Filesystem`, the
        // workspace+build pair for a backend that builds without a device
        // filesystem (`SPEC.md` §11), and a placeholder only in the window
        // before the panes exist at all.
        match &areas.row2 {
            layout::Row2::WorkspaceBuild { workspace, build } => {
                workspace::draw(frame, *workspace, app, palette);
                build::draw(frame, *build, app, palette);
            }
            layout::Row2::Browser(row) => files::draw(frame, row, app, palette),
            layout::Row2::Placeholder(rect) => {
                panels::draw_no_filesystem(frame, *rect, app, palette)
            }
        }
    }

    // Row 3 is one bordered pane for the whole width: the Log/Monitor/
    // Terminal tab strip lives on the pane's own top border (`SPEC.md`
    // §11), like the Ratatui `Tabs` example, and the selected tab's body
    // fills the pane.
    match app.log_tab {
        LogTab::Log => panels::draw_logs(frame, areas.row3, app, palette),
        LogTab::Monitor => monitor::draw(frame, areas.row3, app, palette),
        LogTab::Terminal => terminal::draw(frame, areas.row3, app, palette),
    }
    panels::draw_log_tabs(frame, areas.row3, app, palette);
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
    frame.render_widget(
        Paragraph::new(Line::from(backend_spans(app, palette))),
        area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(device_status(app, palette))).alignment(Alignment::Right),
        area,
    );

    let Some((center, x)) = header_center(area, app, palette) else {
        return;
    };
    let center_width = spans_width(&center) as u16;
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

/// The header's centered zone: the project spans that fit between the
/// backend and device sides, and the column they start at --- the one
/// computation [`draw_header`] and [`header_project_name_rect`] must never
/// disagree on, or a click aimed at the drawn name would silently miss it.
fn header_center(area: Rect, app: &App, palette: Palette) -> Option<(Vec<Span<'static>>, u16)> {
    let left_width = spans_width(&backend_spans(app, palette));
    let right_width = spans_width(&device_status(app, palette));
    let zone_start = left_width + 1;
    let zone = (area.width as usize)
        .saturating_sub(right_width + 1)
        .saturating_sub(zone_start);
    let center = project_spans(app, palette, zone)?;
    let center_width = spans_width(&center) as u16;
    let x = (area.width.saturating_sub(center_width) / 2)
        .max(zone_start as u16)
        .min(area.width.saturating_sub(right_width as u16 + center_width));
    Some((center, x))
}

/// The header's left side: the badge, then the backend icon and name. The
/// icon is decoration (`IconSet::shows_decorations`) --- the same mark the
/// home screen's rows carry, hidden whole by the `none` icon set so the
/// name alone says the backend there too --- and under the Nerd set only
/// MicroPython gets its own glyph (the Python logo,
/// `IconSet::python`); Zephyr keeps its `◆` in every set.
fn backend_spans(app: &App, palette: Palette) -> Vec<Span<'static>> {
    let icons = app.icon_set();
    let (icon, backend) = match app.manager.selected_kind() {
        Some(BackendKind::Zephyr) => ("◆", BackendKind::Zephyr.display_name()),
        Some(BackendKind::MicroPython) => (icons.python(), BackendKind::MicroPython.display_name()),
        None => ("◇", "none"),
    };
    let mut spans = vec![
        Span::styled(
            " ChipTUI ",
            Style::new().fg(palette.bg).bg(palette.accent).bold(),
        ),
        Span::raw(" "),
    ];
    if icons.shows_decorations() && !icon.is_empty() {
        spans.push(Span::styled(icon, Style::new().fg(palette.accent)));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(backend, Style::new().fg(palette.fg).bold()));
    spans.push(missing_tools(app, palette));
    spans
}

/// The header's missing-tools badge: a red `⚠ N` beside the backend name,
/// counting the selected backend's required tools that are not runnable
/// (`App::tool_status`, the same definition the startup warning logs).
/// Shown only when something is missing --- an all-present toolchain is
/// the silent norm, and the names stay in the log warning where they fit.
fn missing_tools(app: &App, palette: Palette) -> Span<'static> {
    let missing = app
        .tool_status()
        .iter()
        .filter(|(_, available)| !available)
        .count();
    if missing == 0 {
        Span::raw("")
    } else {
        Span::styled(
            format!("  ⚠ {missing}"),
            Style::new().fg(palette.error).bold(),
        )
    }
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
    let mut spans: Vec<Span<'static>> = highlighted_line(
        "Project",
        Style::new().fg(palette.muted),
        shortcut_highlight_style(palette),
        shortcut_letter(app, 'p'),
    )
    .spans;
    spans.push(Span::raw(" "));
    spans.push(Span::styled(name, Style::new().fg(palette.fg).bold()));
    Some(spans)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.width()).sum()
}

/// The rect the header draws the project *name* in, `None` when no name is
/// drawn (no project answered, or the free zone cannot hold one). Built
/// from the same spans [`draw_header`] renders, so the mouse hit-test and
/// the frame cannot disagree about where the name sits; only the name's
/// own span is returned --- "Project" prefix excluded --- because that is
/// the part a click means.
pub(crate) fn header_project_name_rect(area: Rect, app: &App, palette: Palette) -> Option<Rect> {
    let (center, x) = header_center(area, app, palette)?;
    // `Project ` leads the name; its width is the offset of the name span.
    // The label itself may be split into more than one span (the shortcuts
    // overlay highlighting its `p`), so the name is always the *last* span
    // and everything before it is the prefix, regardless of count.
    let name = center
        .last()
        .expect("project_spans always appends a name span");
    let prefix: usize = center[..center.len() - 1].iter().map(Span::width).sum();
    Some(Rect {
        x: x + prefix as u16,
        y: area.y,
        width: name.width() as u16,
        height: 1,
    })
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
/// It shows only what a user cannot guess, so it is short --- but more
/// hints than columns is still possible on a narrow terminal, and the one
/// that must survive is the last (on the dashboard, `?` help --- the way
/// to the rest; an overlay with no per-variant hint of its own carries its
/// own `?`/`F1` help entry last for the same reason, `App::shortcuts`).
/// So hints are dropped whole, from the *middle*, rather than letting the
/// line truncate mid-word: a cut-off " ?  he" is worse than one fewer
/// hint.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    const KEEP_LAST: usize = 1;

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
    app.view == View::Dashboard
        && app.focus == focus
        && app.overlay.is_none()
        // The shortcuts overlay dims *every* pane, the one that was
        // focused included: its whole point is showing every reachable
        // pane's initial at once, not just the cursor's.
        && !app.shortcuts_overlay_active
}

/// Whether a dialog currently owns the screen --- the flash view, or any
/// overlay over the dashboard. The whole dashboard reads as dimmed then,
/// output panes included: what the user is answering is the dialog, and
/// everything behind it is context.
fn dashboard_behind_dialog(app: &App) -> bool {
    app.view != View::Dashboard || app.overlay.is_some() || app.shortcuts_overlay_active
}

/// Style for a dashboard pane's content: unchanged when focused, dimmed
/// otherwise. Ratatui has no dedicated "darken the background" primitive ---
/// a terminal buffer has no compositing/alpha to blend against, so this is
/// the closest equivalent: `Modifier::DIM` set at the widget level, which
/// cascades onto every already-colored `Span` drawn inside instead of
/// requiring each one to know about focus.
///
/// This is the *selection* rule, and it belongs to panes whose content is a
/// list the cursor walks (the file columns, the Project checklist): dimming
/// the ones the cursor has left is what makes the live one obvious. Panes
/// that carry output use [`output_style`] instead.
fn content_style(focused: bool) -> Style {
    if focused {
        Style::default()
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

/// Style for an *output* pane's content --- the Log feed and the Monitor
/// console: dimmed only while a dialog owns the screen, never merely
/// because another pane holds the cursor.
///
/// A log entry does not become less worth reading when the cursor moves to
/// the build pane, and the dashboard deliberately parks focus *there* while
/// a command streams (`BuildPanel`'s rule: the Monitor tab is shown, the
/// cursor waits on `Stop`). Dimming on focus alone therefore dims exactly
/// what the user is watching, for the whole length of the build. The focus
/// indicator stays where it belongs --- the pane's border and its tab strip
/// (`pane_border`, `panels::draw_log_tabs`).
fn output_style(app: &App) -> Style {
    content_style(!dashboard_behind_dialog(app))
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

/// A bordered block that shows whether it holds focus, and --- while the
/// shortcuts overlay is up --- highlights `shortcut`'s letter in the title
/// (`None` for a pane the overlay never targets, e.g. Device Info).
fn pane_block(
    title: &str,
    focused: bool,
    palette: Palette,
    shortcut: Option<char>,
) -> Block<'static> {
    pane_border(focused, palette).title(title_span(title, focused, palette, shortcut))
}

/// `Some(letter)` only while the shortcuts overlay is up and `letter` is
/// currently one of its live targets (`App::is_shortcut_active`) --- a pane
/// whose jump would not actually do anything (e.g. Environment with every
/// question already answered) never claims a highlight nobody can act on.
fn shortcut_letter(app: &App, letter: char) -> Option<char> {
    app.is_shortcut_active(letter).then_some(letter)
}

/// The color a pane's border carries: the theme's accent while the pane
/// holds focus, muted otherwise. Shared by [`pane_border`] and the tab strips
/// that draw *over* a pane's top border row (`panels::draw_log_tabs`,
/// `files::draw_device_tabs`): a strip's base style paints every cell of that
/// row --- the border rules included --- so it must restate the border's own
/// colors or the focused frame reads accent everywhere except its top edge.
pub(crate) fn border_style(focused: bool, palette: Palette) -> Style {
    if focused {
        Style::new().fg(palette.accent)
    } else {
        Style::new().fg(palette.muted)
    }
}

/// The focused pane's background: the theme's accent blended a very long
/// way toward the theme's own background (1/64 --- almost imperceptible by
/// design; a whole pane interior is washed, and the home rows' 3/16 resting
/// tint would shout at that scale). The accent keeps the tint on the
/// theme's own hue --- a theme switch recolors the focus the same way it
/// recolors everything else --- while the distance keeps it a whisper: two
/// adjacent panes differ by a channel step or two, felt rather than read.
/// Every pane reaches this through [`render_pane`]/[`paint_focus_wash`],
/// applied over the inner area only, so the rule cannot drift between them.
pub(crate) fn focused_pane_bg(palette: Palette) -> Color {
    crate::backend::blend(palette.accent, palette.bg, 1, 64)
}

/// The focused pane's tint painted over `inner` only: the borders keep
/// the terminal's own background, so the frame stays a line drawing and
/// the tint reads as a lit interior. Rendered *before* the pane's content
/// --- anything the content draws covers what it owns (a selected row's
/// `palette.selection`, the terminal emulator's per-cell colors), and
/// every cell it leaves untouched keeps the tint.
pub(crate) fn paint_focus_wash(frame: &mut Frame, inner: Rect, focused: bool, palette: Palette) {
    if focused && inner.width > 0 && inner.height > 0 {
        frame.render_widget(
            Block::default().style(Style::new().bg(focused_pane_bg(palette))),
            inner,
        );
    }
}

/// The standard pane render: the block over `area`, then the
/// [`paint_focus_wash`] over its inner rect, so the content rendered
/// afterwards sits on the tint instead of under it. Returns the inner
/// rect the content renders into (the same `block.inner(area)` the call
/// sites used to compute by hand).
pub(crate) fn render_pane(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    focused: bool,
    palette: Palette,
) -> Rect {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    paint_focus_wash(frame, inner, focused, palette);
    inner
}

/// An untitled bordered block that shows whether it holds focus: row 3's
/// pane, whose top border row belongs to the Log/Monitor tab strip and
/// whose bottom border row carries the active tab's status at its right
/// --- see `panels::draw_log_tabs`. The focused pane's subtle background
/// tint is painted by [`render_pane`]/[`paint_focus_wash`] over the
/// *inner* area only, never by the block itself.
pub(crate) fn pane_border(focused: bool, palette: Palette) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border_style(focused, palette))
}

/// A pane/tab title with its leading glyph: `<glyph> <title>` when the
/// active icon set has one for this surface, the bare `title` when it does
/// not (`none`, or a surface with no glyph). The empty-glyph case is the
/// rule, not a patch: `none` hides every pane/tab decoration whole
/// ([`IconSet::shows_decorations`](crate::icons::IconSet)), so a title
/// never keeps a blank column the way a *button* deliberately does
/// (geometry there is sacred, titles have no budget).
pub(crate) fn pane_title(glyph: &str, title: &str) -> String {
    if glyph.is_empty() {
        title.to_string()
    } else {
        format!("{glyph} {title}")
    }
}

/// A pane's title with its shortcut number prefixed (`1 ▣ Files: …`): the
/// number at the title's leading edge is the digit that jumps straight to
/// the pane (`App::pane_for_number`), and the numbers are **fixed per pane
/// position** --- 1 Environment, 2 Device Info, 3/4 the working row's two
/// panes, 5 row 3 --- so they are the same in every backend and worth
/// memorizing, unlike the `Tab` tour's dynamic order (which the Project
/// and Device Info panes sit off entirely). A pane that is not focusable
/// right now keeps its plain title. The prefix rides *ahead* of the glyph
/// on purpose: titles clip at their tail, and the number must be the one
/// cell a narrow pane never eats.
pub(crate) fn numbered_title(app: &App, focus: Focus, glyph: &str, title: &str) -> String {
    let base = pane_title(glyph, title);
    match app.pane_number(focus) {
        Some(number) => format!("{number} {base}"),
        None => base,
    }
}

fn title_span(
    title: &str,
    focused: bool,
    palette: Palette,
    shortcut: Option<char>,
) -> Line<'static> {
    let title_style = if focused {
        Style::new().fg(palette.accent).add_modifier(Modifier::BOLD)
    } else {
        muted_style(palette)
    };
    highlighted_line(
        &format!(" {title} "),
        title_style,
        shortcut_highlight_style(palette),
        shortcut,
    )
}

/// The style a shortcut-overlay initial is drawn in, wherever it appears
/// (a pane's title, or a tab strip's label) --- accent, bold and underlined
/// so it reads as "press this" rather than merely "this is selected".
pub(crate) fn shortcut_highlight_style(palette: Palette) -> Style {
    Style::new()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

/// `text` as a `Line`, styled `base` throughout except `shortcut`'s letter
/// (case-insensitive, first occurrence), which gets `highlight` instead ---
/// the shared building block for a pane's title ([`title_span`]) and the
/// two tab strips (`panels::draw_log_tabs`, `files::draw_device_tabs`) that
/// highlight a jump key's initial while the shortcuts overlay is up.
/// `shortcut: None` (the overlay is closed, or this text has no live
/// target) renders `text` plainly.
pub(crate) fn highlighted_line(
    text: &str,
    base: Style,
    highlight: Style,
    shortcut: Option<char>,
) -> Line<'static> {
    if let Some(letter) = shortcut
        && let Some((byte_index, matched)) = text
            .char_indices()
            .find(|(_, c)| c.eq_ignore_ascii_case(&letter))
    {
        let before = text[..byte_index].to_string();
        let after = text[byte_index + matched.len_utf8()..].to_string();
        return Line::from(vec![
            Span::styled(before, base),
            Span::styled(matched.to_string(), highlight),
            Span::styled(after, base),
        ]);
    }
    Line::from(Span::styled(text.to_string(), base))
}

/// The discreet one-column scrollbar shared by the Log and Monitor panes:
/// thin dim track, slightly brighter thumb, no arrow heads. Drawn just inside
/// `inner`'s right edge --- callers reserve that column (the Log pane shrinks
/// its wrap width; the Monitor pane pads its block) so wrapped text never
/// reflows when the bar appears. `content` and `viewport` are visual
/// (post-wrap) rows; `position` is the first visible row. The thumb pins to
/// both ends of the track: at maximum scroll the first visible row is
/// `content - viewport`, while the widget's own `position` scale tops out at
/// `content - 1` (as if the viewport could start on the last item), so the
/// raw offset left the thumb shy of the bottom by a growing gap --- the
/// offset is rescaled into the widget's scale before it is handed over.
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
    let max_offset = content - viewport;
    let top = position.min(max_offset);
    let scaled = top * (content - 1) / max_offset;
    let mut state = ScrollbarState::new(content)
        .viewport_content_length(viewport)
        .position(scaled);
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
