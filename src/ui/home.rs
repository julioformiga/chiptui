//! The home screen's rendering (`SPEC.md` §11).
//!
//! One centered panel: the way to a new project, a live search field, and
//! the recorded projects under it. Each project row is tinted with its
//! backend's color ([`crate::backend::BackendKind::palette`]) so the two
//! kinds are separable before a single word is read; the cursor deepens the
//! tint instead of reversing the row, which is what keeps a colored row
//! legible.
//!
//! Like the dashboard, this is a pure function of state --- everything it
//! needs comes from [`HomeScreen`], and the modal steps draw over it the way
//! `ui::overlay` draws over the panes.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};

use crate::home::{Flow, HomeScreen, Row};
use crate::workspace::{DirRowKind, dir_rows};

use super::{centered, muted_style, selection_style};

/// Widest the panel gets; beyond this the path column stops being scannable.
const MAX_WIDTH: u16 = 100;

/// The screen exists before any [`crate::app::App`] --- `main.rs`'s
/// `home_loop` resolves the theme straight from the user config and passes
/// it down. Named `theme` here (not `palette`) because every row already
/// has a local `palette` of its own kind
/// ([`crate::backend::BackendKind::palette`], the row's backend-identity
/// tint) --- the two are orthogonal and coexist on the same row.
pub fn draw(frame: &mut Frame, screen: &HomeScreen, theme: super::Palette) {
    let area = frame.area();
    let panel = centered(
        area,
        MAX_WIDTH.min(area.width.saturating_sub(4)),
        area.height,
    );

    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(panel);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .title(Span::styled(
            " ChipTUI ",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(body);
    frame.render_widget(block, body);

    let [create_area, search_area, gap, list_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let rows = screen.rows();
    let selected = screen.selected();

    let create_style = if selected == 0 {
        selection_style(theme).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(" + New project ", create_style))),
        create_area,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" 🔍 "),
            Span::styled(screen.query().to_string(), Style::new().fg(theme.fg)),
            Span::styled("▏", Style::new().fg(theme.accent)),
        ])),
        search_area,
    );
    frame.render_widget(Paragraph::new(""), gap);

    if rows.len() == 1 {
        let message = if screen.is_empty() {
            "No projects yet. Create one, or start ChipTUI inside a project directory."
        } else {
            "No project matches the search."
        };
        frame.render_widget(
            Paragraph::new(Line::from(message.fg(theme.muted))),
            list_area,
        );
    } else {
        let width = list_area.width as usize;
        let items: Vec<ListItem> = rows
            .iter()
            .skip(1)
            .enumerate()
            .map(|(index, row)| {
                let Row::Project(entry) = row else {
                    return ListItem::new("");
                };
                // The list widget's own highlight would repaint the whole
                // row in one style; the tint has to survive selection, so
                // the row carries its background itself.
                let palette = entry.backend.palette(theme);
                let background = if index + 1 == selected {
                    palette.tint_selected
                } else {
                    palette.tint
                };
                let base = Style::new().bg(background);
                ListItem::new(Line::from(project_spans(
                    screen,
                    entry,
                    base,
                    palette.accent,
                    theme.muted,
                    width,
                )))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(selected.saturating_sub(1)));
        frame.render_stateful_widget(List::new(items), list_area, &mut state);
    }

    let status = match screen.status() {
        Some(status) => Line::from(status.to_string().fg(theme.warning)),
        None => Line::from(""),
    };
    frame.render_widget(Paragraph::new(status), status_area);

    frame.render_widget(
        Paragraph::new(Line::from(
            " ↑/↓ move · enter open · del forget · esc clear / quit ".fg(theme.muted),
        )),
        footer,
    );

    if let Some(flow) = screen.flow() {
        draw_flow(frame, area, screen, flow, theme);
    }
}

/// `<icon> <backend>  <name>  <path>` --- fixed columns for the first two so
/// the names line up, the path taking whatever is left and losing its head
/// (never its tail: the project's own folder is the identifying part).
/// The `none` icon set drops the `<icon>` column (and its 3 budgeted cells)
/// whole, the same rule the file browser's emoji column follows; under the
/// Nerd set only MicroPython trades its emoji for the Python logo (the
/// header's rule, `IconSet::python`) --- Zephyr keeps its `🔷` in every
/// set, and plain Unicode keeps both emoji.
fn project_spans<'a>(
    screen: &HomeScreen,
    entry: &'a crate::settings::ProjectEntry,
    base: Style,
    accent: Color,
    muted: Color,
    width: usize,
) -> Vec<Span<'a>> {
    const BACKEND_WIDTH: usize = 12;
    const NAME_WIDTH: usize = 22;

    let icons = screen.icons();
    let mark = match (icons, entry.backend) {
        (crate::icons::IconSet::Nerd, crate::backend::BackendKind::MicroPython) => {
            icons.python().to_string()
        }
        _ => entry.backend.icon().to_string(),
    };
    let name = fit(&entry.name, NAME_WIDTH);
    let icon_cols = usize::from(icons.shows_decorations()) * 3;
    let used = 1 + icon_cols + BACKEND_WIDTH + 2 + NAME_WIDTH + 2;
    let path_width = width.saturating_sub(used + 1);
    let path = super::panels::truncate_start(&screen.display_path(&entry.path), path_width);

    let mut spans = vec![
        Span::styled(" ", base),
        Span::styled(
            format!("{:<BACKEND_WIDTH$}", entry.backend.display_name()),
            base.fg(accent).add_modifier(Modifier::BOLD),
        ),
    ];
    if icons.shows_decorations() {
        spans.insert(1, Span::styled(format!("{mark} "), base));
    }
    spans.push(Span::styled("  ", base));
    spans.push(Span::styled(format!("{name:<NAME_WIDTH$}"), base));
    spans.push(Span::styled("  ", base));
    // Padded to the pane's width so the tint reaches the right edge:
    // a row that stops at its text would look like a ragged block.
    spans.push(Span::styled(format!("{path:<path_width$}"), base.fg(muted)));
    spans
}

/// Shortens `text` from the right, keeping the head --- a project's name
/// identifies it from the front, unlike its path.
fn fit(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let kept: String = text.chars().take(max - 1).collect();
    format!("{kept}…")
}

fn draw_flow(
    frame: &mut Frame,
    area: Rect,
    screen: &HomeScreen,
    flow: &Flow,
    theme: super::Palette,
) {
    match flow {
        Flow::CreateDir {
            path,
            selected,
            error,
        } => draw_create_dir(frame, area, path, *selected, error.as_deref(), theme),
        Flow::CreateName {
            parent,
            input,
            error,
        } => draw_create_name(frame, area, screen, parent, input, error.as_deref(), theme),
        Flow::Forget { path, name } => draw_forget(frame, area, screen, path, name, theme),
    }
}

fn modal(title: &str, theme: super::Palette) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ))
}

fn draw_create_dir(
    frame: &mut Frame,
    area: Rect,
    path: &std::path::Path,
    selected: usize,
    error: Option<&str>,
    theme: super::Palette,
) {
    let popup = centered(area, 72, 18);
    frame.render_widget(Clear, popup);
    let block = modal("Where should the project folder go?", theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [path_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("in   ", muted_style(theme)),
            Span::styled(path.display().to_string(), Style::new().fg(theme.fg)),
        ])),
        path_area,
    );

    let (rows, read_error) = dir_rows(path);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row.kind {
            DirRowKind::Use => ListItem::new(Line::from(vec![
                Span::styled("→ ", Style::new().fg(theme.accent)),
                "put it in this directory".fg(theme.fg).bold(),
            ])),
            DirRowKind::Parent | DirRowKind::Dir => ListItem::new(Line::from(Span::styled(
                format!("  {}", row.name),
                Style::new().fg(theme.fg),
            ))),
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(theme)),
        list_area,
        &mut state,
    );

    let footer = match (error, read_error.as_deref()) {
        (Some(error), _) => Line::from(error.to_string().fg(theme.error)),
        (None, Some(read)) => Line::from(read.fg(theme.warning)),
        (None, None) => Line::from("enter: open / accept · ←: up · esc: cancel".fg(theme.muted)),
    };
    frame.render_widget(
        Paragraph::new(footer).wrap(ratatui::widgets::Wrap { trim: false }),
        footer_area,
    );
}

fn draw_create_name(
    frame: &mut Frame,
    area: Rect,
    screen: &HomeScreen,
    parent: &std::path::Path,
    input: &str,
    error: Option<&str>,
    theme: super::Palette,
) {
    let popup = centered(area, 72, 9);
    frame.render_widget(Clear, popup);
    let block = modal("Name the project", theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [in_area, input_area, preview_area, _, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("in   ", muted_style(theme)),
            Span::styled(screen.display_path(parent), Style::new().fg(theme.fg)),
        ])),
        in_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("name ", muted_style(theme)),
            Span::styled(input.to_string(), Style::new().fg(theme.fg)),
            Span::styled("▏", Style::new().fg(theme.accent)),
        ])),
        input_area,
    );
    let preview = if input.trim().is_empty() {
        String::new()
    } else {
        screen.display_path(&parent.join(input.trim()))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("→    ", muted_style(theme)),
            Span::styled(preview, muted_style(theme)),
        ])),
        preview_area,
    );

    let footer = match error {
        Some(error) => Line::from(error.to_string().fg(theme.error)),
        None => Line::from(
            "enter: create the folder · esc: back — the backend is asked next".fg(theme.muted),
        ),
    };
    frame.render_widget(
        Paragraph::new(footer).wrap(ratatui::widgets::Wrap { trim: false }),
        footer_area,
    );
}

fn draw_forget(
    frame: &mut Frame,
    area: Rect,
    screen: &HomeScreen,
    path: &std::path::Path,
    name: &str,
    theme: super::Palette,
) {
    let popup = centered(area, 66, 7);
    frame.render_widget(Clear, popup);
    let block = modal("Remove from the list?", theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = vec![
        Line::from(vec![Span::styled(
            name.to_string(),
            Style::new().fg(theme.fg).bold(),
        )]),
        Line::from(Span::styled(screen.display_path(path), muted_style(theme))),
        Line::from(""),
        Line::from("The folder and its files stay exactly where they are.".fg(theme.muted)),
        Line::from("enter / y: remove · esc / n: keep".fg(theme.muted)),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}
