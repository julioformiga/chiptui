//! Project-files-pane rendering: the backend's shared environment
//! (Zephyr) owns its *files* here --- the environment's questions moved up
//! to the Project pane (`crate::ui::panels`), so the whole pane is the
//! listing (`crate::ui::files`' row/list grammar, reused so it looks like
//! MicroPython's own local pane), with the walked path in the pane's own
//! title. Selected rows highlight full-width. The checklist-row grammar
//! helpers the Project pane now renders live here too (they grew in this
//! module and both panes share them).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, Focus};
use crate::ui::{Palette, dashboard_focused, muted_style, pane_block, selection_style, tilde_path};
use crate::workspace::WorkspacePanel;

/// Draws the full second row for a workspace+build backend: the workspace
/// pane on the left, the project panel (already its own module) on the right.
pub fn draw_row(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    draw(frame, left, app, palette);
    super::build::draw(frame, right, app, palette);
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let Some(panel) = &app.workspace else {
        return;
    };
    let focused = dashboard_focused(app, Focus::Workspace);
    // The pane's own title is the walked path the old embedded bar carried
    // ("blinkety/src/"): with the checklist moved up to the Project pane,
    // the bar would sit between two borders saying nothing, so the border
    // says it instead and the listing gets the whole pane. The prefix never
    // truncates --- only the path shortens, from the left (9 is
    // "Files: " plus the title's own padding spaces).
    let title = format!(
        "Files: {}",
        shorten_start(
            &files_title(panel, app),
            area.width.saturating_sub(9) as usize
        )
    );
    let block = pane_block(&title, focused, palette);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    draw_files_section(frame, inner, app, panel, palette);
}

/// The pane title's walked path: the project's own name concatenated with
/// the directories descended below it, always slash-terminated --- the same
/// shape MicroPython's local pane's title follows now.
fn files_title(panel: &WorkspacePanel, app: &App) -> String {
    let project = panel
        .files_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| tilde_path(&panel.files_root, app.home_dir()));
    panel
        .files_path
        .strip_prefix(&panel.files_root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| format!("{project}/{}/", relative.display()))
        .unwrap_or(format!("{project}/"))
}

/// The project's files, filling the whole pane now that the checklist
/// moved up to the Project pane: the same `row`/`render_list` grammar
/// `crate::ui::files` draws for MicroPython's local pane, reused with no
/// comparison status (`row`'s `None` arm draws no marker column at all ---
/// this pane has no other side to compare against), so an empty listing, an
/// overlong name and a directory marker read identically here. A below-root
/// listing leads with a `[..]` parent row (see
/// [`WorkspacePanel::parent_row`]); the walked path lives in the pane's own
/// title (see [`files_title`]).
fn draw_files_section(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    panel: &WorkspacePanel,
    palette: Palette,
) {
    if let Some(error) = &panel.files_error {
        frame.render_widget(
            Paragraph::new(error.clone().fg(palette.error)).wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let focused = dashboard_focused(app, Focus::Workspace);
    // The `[..]` row leads whenever the listing is below the project root,
    // so `files_cursor` (which addresses drawn rows, 0 first) can be passed
    // straight through as the list's selection.
    let mut items = Vec::with_capacity(panel.files_row_count());
    if panel.parent_row() {
        items.push(super::files::row(
            "[..]", true, 0, None, area.width, palette,
        ));
    }
    items.extend(panel.visible_files().iter().map(|entry| {
        super::files::row(
            &entry.name,
            entry.is_dir,
            entry.size,
            None,
            area.width,
            palette,
        )
    }));
    let cursor = Some(panel.files_cursor);
    super::files::render_list(frame, area, items, cursor, focused, palette);
}

/// The four states a checklist row's mark can carry. `Open` is the dim
/// `□` that says *this needs defining*; `Warn` is the state the Project
/// pane never had and the installer's prerequisite list needs --- an
/// answer that is not what was recommended but does not stop anything
/// (the system Python, when pyenv will provide 3.12 anyway). Keeping it in
/// the shared grammar is what stops the installer from inventing a second
/// row vocabulary next to this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowMark {
    Done,
    Warn,
    Broken,
    Open,
}

impl RowMark {
    /// The glyph and its color. All four are plain BMP characters --- no
    /// Private Use Area, which `tests/no_private_use_glyphs.rs` enforces.
    fn span(self, palette: Palette) -> Span<'static> {
        match self {
            Self::Done => Span::styled("✓", Style::new().fg(palette.success).bold()),
            Self::Warn => Span::styled("⚠", Style::new().fg(palette.warning).bold()),
            Self::Broken => Span::styled("✗", Style::new().fg(palette.error).bold()),
            Self::Open => Span::styled("□", muted_style(palette)),
        }
    }
}

/// One checklist row in the shared grammar: `mark`, a space, the label
/// padded to `label_width`, a space, then the answer. The value is a whole
/// [`Line`], so a row whose answer needs several styled spans (the Project
/// pane's target row) uses the same shape. `label_width` is the one thing
/// that varies: the panes agree on 13 ([`checklist_row`]), the installer's
/// step list needs more room for its sentences.
pub(crate) fn marked_row(
    mark: RowMark,
    label_width: usize,
    label: &str,
    value: Line<'static>,
    palette: Palette,
) -> Line<'static> {
    let mut spans = vec![
        mark.span(palette),
        Span::raw(" "),
        Span::styled(
            format!("{label:<label_width$}"),
            Style::new().fg(palette.fg).bold(),
        ),
        Span::raw(" "),
    ];
    spans.extend(value.spans);
    Line::from(spans)
}

/// One checklist row: `✓` when the answer exists, a dim `□` while the
/// question is still open, a red `✗` when a configured one failed
/// validation --- then the label, then the answer itself. Shared with the
/// project panel --- one checklist grammar across the rows.
pub(crate) fn checklist_row(
    done: bool,
    broken: bool,
    label: &str,
    value: Line<'static>,
    palette: Palette,
) -> Line<'static> {
    let mark = match (broken, done) {
        (true, _) => RowMark::Broken,
        (false, true) => RowMark::Done,
        (false, false) => RowMark::Open,
    };
    marked_row(mark, LABEL_WIDTH, label, value, palette)
}

/// The right-hand side of a checklist row: the answer when there is one,
/// a red `!` when a configured one failed validation, a yellow `?` while
/// the question is open.
pub(crate) fn answer_value(
    answer: Option<String>,
    broken: bool,
    width: u16,
    palette: Palette,
) -> Line<'static> {
    if let Some(answer) = answer {
        Line::from(
            shorten_start(&answer, value_budget(width))
                .fg(palette.success)
                .bold(),
        )
    } else if broken {
        Line::from("!".fg(palette.error).bold())
    } else {
        Line::from("?".fg(palette.warning).bold())
    }
}

/// Characters a checklist row's value may occupy: the mark, the two
/// spaces and the 13-column label (`Projects base`, the longest) take 16.
/// The merged Board · Shield row needs the room most: its two names and
/// their separator share this budget.
pub(crate) fn value_budget(width: u16) -> usize {
    (width as usize).saturating_sub(LABEL_WIDTH + 3).max(8)
}

/// The label column both panes share: `Projects base` is the longest of
/// them.
pub(crate) const LABEL_WIDTH: usize = 13;

/// The muted key a pane's pinned last line labels itself with (`state`,
/// `last`, `env`), in the same 6 columns across both panes.
pub(super) fn label(text: &str, palette: Palette) -> Span<'static> {
    Span::styled(format!("{text:<6}"), muted_style(palette))
}

/// Renders one navigable row, full-width in the theme's selection colors
/// when selected, and returns the next row's y. Rows past the pane's bottom
/// are dropped.
pub(crate) fn render_row(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    line: Line<'static>,
    selected: bool,
    palette: Palette,
) -> u16 {
    if y >= area.bottom() {
        return y;
    }
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    };
    if selected {
        frame.render_widget(line.style(selection_style(palette)), rect);
    } else {
        frame.render_widget(line, rect);
    }
    y + 1
}

/// Shortens from the left: a path's tail (its distinctive part) matters
/// more than its `/tmp` prefix.
fn shorten_start(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let tail: String = text.chars().skip(length - (max - 1)).collect();
    format!("…{tail}")
}
