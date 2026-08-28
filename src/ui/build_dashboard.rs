//! Drawing the build dashboard window.
//!
//! The geometry is [`crate::ui::layout::build_dashboard`] --- one
//! definition, shared with the click hit-testing, the contract every modal
//! in this app now keeps. The window is the docs pickers' two-pane body
//! with a tab strip added, and the strip rides the modal's *top border* the
//! way row 3's Log/Monitor/Terminal strip rides its pane's: at the declared
//! 80x32 minimum the body has 23 rows, and a strip that has a border to sit
//! on should not also take one of them.
//!
//! Because the strip owns the top border, the modal carries no title of its
//! own --- exactly like row 3's pane, and for the same reason. The strip's
//! leading glyph and its tab names are the identity.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Tabs};
use ratatui::{Frame, symbols};

use crate::app::{App, DocsFocus};
use crate::build_dashboard::{DashboardTab, DetailLine, Marker, Row};
use crate::ui::{Palette, draw_scrollbar, muted_style, selection_style};

use super::overlay::{labelled, pane, wrap_words};

/// The indent one tree level costs. Two columns: enough to read as
/// nesting, cheap enough that a depth-13 memory tree still shows names.
const INDENT: usize = 2;

/// Columns a row's label keeps whatever its trailing value wants. A row
/// without a name is not a row.
const MIN_LABEL: usize = 16;

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    let areas = crate::ui::layout::build_dashboard(area);
    frame.render_widget(Clear, areas.popup);
    // Untitled: the strip below sits on this border and is the title.
    frame.render_widget(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(palette.accent)),
        areas.popup,
    );

    draw_strip(frame, areas.strip, app, palette);
    draw_filter(frame, areas.filter, app, palette);
    frame.render_widget(
        Paragraph::new(hint(app).fg(palette.muted)).wrap(ratatui::widgets::Wrap { trim: false }),
        areas.hint,
    );

    let rows = app.build_dashboard.rows();
    draw_list(frame, areas.list, app, palette, &rows);
    draw_details(frame, areas.details, app, palette);
}

/// The tab strip, drawn onto the modal's top border after the block so it
/// sits *on* it --- `panels::draw_log_tabs`' own technique.
fn draw_strip(frame: &mut Frame, strip: Rect, app: &App, palette: Palette) {
    if strip.width == 0 {
        return;
    }
    let focused = app.build_dashboard.focus == DocsFocus::List;
    let active = if focused {
        Style::new()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    };
    // The titles come from `App::dashboard_strip_tabs`, the same builder
    // the click hit-testing walks --- writing them twice is how a strip's
    // drawn and clickable widths drift apart.
    let titles: Vec<Line> = app
        .dashboard_strip_tabs()
        .into_iter()
        .map(|(tab, title)| {
            let style = if tab == app.build_dashboard.tab {
                active
            } else {
                muted_style(palette)
            };
            // The window's mark leads the first title; it is part of that
            // title's width, and drawn muted whichever tab is active.
            match title.split_once(' ') {
                Some((glyph, label)) if tab == DashboardTab::ALL[0] && label == tab.label() => {
                    Line::from(vec![
                        Span::styled(format!("{glyph} "), muted_style(palette)),
                        Span::styled(label.to_string(), style),
                    ])
                }
                _ => Line::from(Span::styled(title, style)),
            }
        })
        .collect();
    frame.render_widget(
        Tabs::new(titles)
            .divider(symbols::DOT)
            .select(app.build_dashboard.tab.index()),
        strip,
    );
}

/// The filter field, with the tab's own count riding the right edge.
fn draw_filter(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let search = app.icon_set().search();
    let mut field = Vec::new();
    if !search.is_empty() {
        field.push(Span::styled(format!("{search} "), muted_style(palette)));
    }
    field.push(Span::styled(
        app.build_dashboard.pane().input.clone(),
        Style::new().fg(palette.fg),
    ));
    field.push(Span::styled("\u{258f}", Style::new().fg(palette.accent)));
    frame.render_widget(Paragraph::new(Line::from(field)), area);

    let count = counts(app);
    if count.is_empty() {
        return;
    }
    let width = count.chars().count() as u16;
    if width >= area.width {
        return;
    }
    frame.render_widget(
        Paragraph::new(count.fg(palette.muted)),
        Rect {
            x: area.x + area.width - width,
            width,
            ..area
        },
    );
}

fn counts(app: &App) -> String {
    let rows = app.build_dashboard.rows().len();
    match app.build_dashboard.tab {
        DashboardTab::Kconfig => format!("{rows} symbols"),
        DashboardTab::ElfStats => format!("{rows} sections"),
        DashboardTab::DeviceTree => format!("{rows} nodes"),
        DashboardTab::Memory if rows > 0 => format!("{rows} rows"),
        _ => String::new(),
    }
}

fn hint(app: &App) -> String {
    let base = "ctrl+←/→ tabs · tab list/details · esc closes";
    match app.build_dashboard.tab {
        // The one row in this window that runs something says so, because
        // it is also the only row `Enter` does not merely expand.
        DashboardTab::Memory if app.build_dashboard.selected_is_prompt() => {
            format!("{base} · enter generates the report")
        }
        DashboardTab::Memory | DashboardTab::DeviceTree
            if app.build_dashboard.pane().input.trim().is_empty() =>
        {
            format!("{base} · →/← open and close, typing flattens the tree")
        }
        _ => format!("{base} · type to filter"),
    }
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette, rows: &[Row]) {
    let focused = app.build_dashboard.focus == DocsFocus::List;
    let block = pane(app.build_dashboard.tab.label(), focused, palette);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if rows.is_empty() {
        let note = app
            .build_dashboard
            .empty_reason()
            .unwrap_or("nothing to show");
        frame.render_widget(
            Paragraph::new(note.to_string().fg(palette.muted))
                .wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
        app.dashboard_list_offset = 0;
        return;
    }

    // The scrollbar's column is reserved whatever happens, so the trailing
    // column never shifts when the bar appears --- the file panes' rule.
    let view = Rect {
        width: inner.width.saturating_sub(1),
        ..inner
    };
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| ListItem::new(row_line(row, view.width as usize, palette)))
        .collect();
    let selected = app.build_dashboard.pane().selected.min(rows.len() - 1);
    // Seeded from the offset the previous frame settled on and published
    // back below: a fresh `ListState` re-anchors the view, which makes a
    // click on a visible row jump.
    let mut state = ListState::default()
        .with_offset(app.dashboard_list_offset.min(rows.len() - 1))
        .with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
        view,
        &mut state,
    );
    app.dashboard_list_offset = state.offset();
    draw_scrollbar(
        frame,
        inner,
        rows.len(),
        inner.height as usize,
        state.offset(),
        palette,
    );
}

/// One row: marker, indent, label, and the trailing value flush right.
fn row_line(row: &Row, width: usize, palette: Palette) -> Line<'static> {
    let marker = match row.marker {
        Marker::Expanded => "\u{25be}",
        Marker::Collapsed => "\u{25b8}",
        Marker::None => " ",
    };
    let indent = " ".repeat(row.depth * INDENT);
    let head = format!("{indent}{marker} ");
    // The trailing value yields to the label, not the other way round: a
    // Summary row whose value is an absolute path would otherwise take the
    // whole width and leave the row nameless --- and the full value is in
    // the details pane regardless.
    let trailing = truncate(
        &row.trailing,
        width
            .saturating_sub(head.chars().count())
            .saturating_sub(MIN_LABEL + 1),
    );
    let trailing_width = trailing.chars().count();
    // The label gets what is left, cut from the tail rather than wrapped:
    // these lists are walked, and a row that reflows moves the rows below it.
    let budget =
        width
            .saturating_sub(head.chars().count())
            .saturating_sub(if trailing_width == 0 {
                0
            } else {
                trailing_width + 1
            });
    let label = truncate(&row.label, budget);
    let used = head.chars().count() + label.chars().count() + trailing_width;
    let gap = width.saturating_sub(used);

    let label_style = if row.dimmed {
        muted_style(palette)
    } else {
        Style::new().fg(palette.fg)
    };
    let mut spans = vec![
        Span::styled(head, muted_style(palette)),
        Span::styled(label, label_style),
    ];
    if trailing_width > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(trailing, muted_style(palette)));
    }
    Line::from(spans)
}

fn truncate(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    if budget <= 1 {
        return text.chars().take(budget).collect();
    }
    let mut cut: String = text.chars().take(budget - 1).collect();
    cut.push('\u{2026}');
    cut
}

fn draw_details(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    let focused = app.build_dashboard.focus == DocsFocus::Details;
    let block = pane("Details", focused, palette);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let view = Rect {
        width: inner.width.saturating_sub(1),
        ..inner
    };
    let width = view.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    for line in app.build_dashboard.details() {
        match line {
            DetailLine::Heading(text) => lines.push(Line::from(Span::styled(
                text,
                Style::new().fg(palette.fg).add_modifier(Modifier::BOLD),
            ))),
            DetailLine::Field { label, value } => {
                lines.extend(labelled(&label, &value, width, palette));
            }
            DetailLine::Text(text) => {
                for wrapped in wrap_words(&text, width) {
                    lines.push(Line::from(wrapped));
                }
            }
            DetailLine::Blank => lines.push(Line::from("")),
        }
    }

    // The viewport is published for the key handler's paging, and the
    // offset is clamped here, where the wrapped length is known.
    app.dashboard_viewport = inner.height as usize;
    let total = lines.len();
    let max_scroll = total.saturating_sub(app.dashboard_viewport);
    let start = (app.build_dashboard.pane().scroll as usize).min(max_scroll);
    let visible = lines.split_off(start);
    frame.render_widget(Paragraph::new(visible), view);
    draw_scrollbar(frame, inner, total, app.dashboard_viewport, start, palette);
}
