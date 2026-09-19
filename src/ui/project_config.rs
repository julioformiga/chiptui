//! The project configuration screen (`Overlay::ProjectConfig`).
//!
//! The window is the file, rendered. Whoever opens it commits
//! `chiptui.toml` and reads the diff afterwards, so the left column carries
//! the file's own key names and the right column --- width the old single
//! list wasted --- earns its place by holding the two things a key cannot
//! say on one line: **where its current answer comes from** when the file
//! is silent, and **the literal line** an unapplied answer will write.
//!
//! Above both, the backend is a pair of cards rather than a value: it is a
//! choice between two worlds of tooling, and each carries its own mark and
//! its own semantic colour --- `success` for MicroPython, `info` for Zephyr,
//! blended toward the theme's background. That is the vocabulary the home
//! screen already uses to tell the two kinds apart at a glance, and reusing
//! it is what makes the card and the project row read as the same fact.
//!
//! The list itself is one bordered pane, the Details column's sibling: a
//! heading with a rule and its answered count is the only delimiter between
//! sections, and General scrolls by exactly like the backend's own groups.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::layout::ProjectConfigAreas;
use super::{Palette, draw_scrollbar, muted_style, selection_style};
use crate::app::App;
use crate::backend::BackendKind;
use crate::project_config::{
    Cursor, Destination, Notice, ProjectConfigPanel, ProjectConfigRow, RowKind, backend_summary,
    choice_label,
};

/// Where a row's value starts, so every answer lines up under the one above
/// however long its label is ("Auto-confirm image", at 18, is the longest).
const KEY_WIDTH: usize = 22;

pub(super) fn draw(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    let areas = super::layout::project_config(area);
    if app.project_config.is_none() {
        return;
    }

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette.accent))
        .title(Span::styled(
            " Project configuration ",
            Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(Clear, areas.popup);
    frame.render_widget(block, areas.popup);

    draw_header(frame, &areas, app, palette);
    draw_cards(frame, &areas, app, palette);
    draw_rows(frame, &areas, app, palette);
    draw_details(frame, &areas, app, palette);
    draw_footer(frame, &areas, app, palette);
}

/// The file being edited, and one line of state under it: the error if
/// there is one, the notice if there is one, else what the window is for.
fn draw_header(frame: &mut Frame, areas: &ProjectConfigAreas, app: &App, palette: Palette) {
    let Some(panel) = app.project_config.as_ref() else {
        return;
    };
    let width = areas.header.width as usize;
    let suffix = if panel.file_exists() {
        String::new()
    } else {
        "   new file".to_string()
    };
    // Cut from the left: a path's tail --- the project and the file --- is
    // what identifies it, and the `/home/...` in front carries nothing.
    let path = super::overlay::shorten_tail(
        &panel.path().display().to_string(),
        width.saturating_sub(suffix.chars().count()),
    );

    let (state, style) = match (panel.error(), panel.notice()) {
        (Some(error), _) => (error.to_string(), Style::new().fg(palette.error)),
        (None, Some(Notice::Done(text))) => (text.to_string(), Style::new().fg(palette.success)),
        (None, Some(Notice::Lost(text))) => (text.to_string(), Style::new().fg(palette.warning)),
        (None, None) => (
            "Nothing is written until you apply.".to_string(),
            muted_style(palette),
        ),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(path, Style::new().fg(palette.fg)),
                Span::styled(suffix, muted_style(palette)),
            ]),
            Line::from(Span::styled(state, style)),
        ]),
        areas.header,
    );
}

/// The backend choice: one card per backend, side by side.
fn draw_cards(frame: &mut Frame, areas: &ProjectConfigAreas, app: &App, palette: Palette) {
    let Some(panel) = app.project_config.as_ref() else {
        return;
    };
    let icons = app.icon_set();
    let on_cards = panel.cursor() == Cursor::Cards;

    for (index, rect) in areas.cards.iter().enumerate() {
        let Some(kind) = BackendKind::ALL.get(index).copied() else {
            continue;
        };
        let chosen = panel.chosen() == Some(kind);
        let backend = kind.palette(palette);
        // A chosen card is drawn in its backend's own colour and filled
        // with its tint; the others keep the frame's muted rules. The
        // cursor deepens the fill rather than reversing it --- the home
        // screen's rule, and for its reason: a painted row cannot also be
        // inverted.
        let border = if chosen {
            Style::new().fg(backend.accent)
        } else {
            muted_style(palette)
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border);
        let inner = block.inner(*rect);
        frame.render_widget(block, *rect);
        if chosen {
            let fill = if on_cards {
                backend.tint_selected
            } else {
                backend.tint
            };
            frame.buffer_mut().set_style(inner, Style::new().bg(fill));
        }

        // The home screen's own mark rule, so a card and the project row it
        // will produce carry the same glyph: the two-cell emoji under plain
        // Unicode, a single-width Nerd mark under `nerd`, nothing under
        // `none` --- and `icon_column` centres the narrow one over the wide
        // one's span so both cards' names start in the same column.
        let (mark, single_cell) = match (icons, kind) {
            (crate::icons::IconSet::Nerd, BackendKind::MicroPython) => (icons.python(), true),
            (crate::icons::IconSet::Nerd, BackendKind::Zephyr) => (icons.zephyr(), true),
            _ => (kind.icon(), false),
        };
        let mut title = Vec::new();
        if icons.shows_decorations() {
            title.push(Span::styled(
                super::icon_column(mark, single_cell),
                Style::new().fg(backend.accent),
            ));
        }
        title.push(Span::styled(
            kind.display_name(),
            Style::new()
                .fg(if chosen { palette.fg } else { palette.muted })
                .add_modifier(Modifier::BOLD),
        ));
        let summary = Span::styled(
            backend_summary(kind),
            if chosen {
                Style::new().fg(palette.fg)
            } else {
                muted_style(palette)
            },
        );
        frame.render_widget(
            Paragraph::new(vec![Line::from(title), Line::from(summary)]),
            Rect {
                x: inner.x + 1,
                width: inner.width.saturating_sub(2),
                ..inner
            },
        );
    }

    // The row under the strip carries the window's one standing lesson.
    // Nothing chosen yet: what to do --- an empty state's whole job. Once
    // chosen: the precedence the details pane's "Current" block then shows
    // per row, stated once here so the column never has to teach it.
    let hint = if panel.chosen().is_none() {
        "Pick one to start.".to_string()
    } else {
        format!(
            "more specific wins:  {}  >  user config  >  defaults",
            crate::project::config::FILE_NAME
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, muted_style(palette))))
            .alignment(ratatui::layout::Alignment::Center),
        areas.hint,
    );
}

/// The key list: one bordered pane, the Details column's sibling, with
/// sections, then their keys, inside it.
fn draw_rows(frame: &mut Frame, areas: &ProjectConfigAreas, app: &App, palette: Palette) {
    let Some(panel) = app.project_config.as_ref() else {
        return;
    };
    // The keyboard's whereabouts ride the border, the Details pane's own
    // grammar in mirror: accent while the list drives, muted once `Tab`
    // hands the arrows to the details.
    let focused = panel.details_focus() == crate::app::DocsFocus::List;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(super::border_style(focused, palette));
    let inner = block.inner(areas.list);
    frame.render_widget(block, areas.list);

    // The list reserves the scrollbar's column whether or not a bar is
    // drawn, so nothing reflows when one appears (`ui::files::list_view`'s
    // rule).
    let body = Rect {
        width: inner.width.saturating_sub(1),
        ..inner
    };
    let items: Vec<ListItem> = panel
        .rows()
        .iter()
        .map(|row| ListItem::new(row_line(app, *row, body.width as usize, palette)))
        .collect();
    let selected = match panel.cursor() {
        Cursor::Row(index) => Some(index),
        Cursor::Cards => None,
    };
    let mut state = ListState::default().with_selected(selected.or(Some(0)));
    frame.render_stateful_widget(
        List::new(items).highlight_style(if selected.is_some() {
            selection_style(palette)
        } else {
            Style::new()
        }),
        body,
        &mut state,
    );
    draw_scrollbar(
        frame,
        inner,
        panel.rows().len(),
        body.height as usize,
        state.offset(),
        palette,
    );
}

/// One line of the list.
fn row_line(app: &App, row: ProjectConfigRow, width: usize, palette: Palette) -> Line<'static> {
    let panel = app.project_config.as_ref();
    if let ProjectConfigRow::Heading(section) = row {
        // The heading opens its section: the title, a rule, and the
        // answered count riding the rule's right end.
        let title = format!(" {} ", section.title());
        let (total, answered) = panel.map_or((0, 0), |panel| {
            let rows = panel.rows().iter().filter(|member| {
                !matches!(member, ProjectConfigRow::Heading(_)) && member.section() == section
            });
            let mut total = 0;
            let mut answered = 0;
            for member in rows {
                total += 1;
                if panel.saved(*member).is_some()
                    || panel.pending_for(*member).is_some()
                    || app.project_config_fallback(*member).is_some()
                {
                    answered += 1;
                }
            }
            (total, answered)
        });
        let count = format!("{answered}/{total}");
        let rule = "─".repeat(width.saturating_sub(title.chars().count() + count.len() + 3));
        return Line::from(vec![
            Span::raw(" "),
            Span::styled(
                title,
                Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(rule, muted_style(palette)),
            Span::styled(format!(" {count} "), muted_style(palette)),
        ]);
    }

    // The glyph column: exactly one mark per row, naming the *control* the
    // row is answered through --- a folder for a path picker, ✎ for free
    // text, ☰ for a fixed set, a chip for a board target --- so the
    // same-shaped answers line up and a row's interaction is read off the
    // mark. A row's *state* the value column says in colour: warning for an
    // answer waiting to be applied, success for one written into the file,
    // muted for an inherited or empty one.
    let pending = panel.and_then(|panel| panel.pending_for(row));
    let saved = panel.and_then(|panel| panel.saved(row));
    let icons = app.icon_set();
    // `icon_column` centres the single-cell marks over the two-cell emoji's
    // span so every row's label starts in the same column; reports carry no
    // glyph and pay the column as blank, keeping the alignment.
    let glyph = if icons.shows_decorations() {
        match row.picker_kind() {
            Some(crate::path_picker::PickerKind::Directory) => Some((
                icons.directory(),
                matches!(icons, crate::icons::IconSet::Nerd),
            )),
            Some(crate::path_picker::PickerKind::File(_)) => {
                Some((icons.file(), matches!(icons, crate::icons::IconSet::Nerd)))
            }
            None if row.uses_target_picker() => Some((icons.microchip(), true)),
            None if matches!(row.kind(), RowKind::Choice(_)) => Some((icons.choice(), true)),
            None if matches!(row.kind(), RowKind::Text) => Some((icons.text_edit(), true)),
            None => None,
        }
    } else {
        None
    };
    let label = panel.map_or_else(
        || row.label().to_string(),
        |panel| display_label(panel, row),
    );
    let mut spans = vec![Span::raw(" ")];
    if icons.shows_decorations() {
        spans.push(match glyph {
            Some((mark, single_cell)) => {
                Span::styled(super::icon_column(mark, single_cell), muted_style(palette))
            }
            None => Span::raw("   "),
        });
    }
    spans.push(Span::styled(
        format!("{label:<KEY_WIDTH$}"),
        Style::new().fg(palette.fg),
    ));
    let budget = width.saturating_sub(KEY_WIDTH + 5);

    // A row being typed into shows the buffer with a block cursor after it
    // --- the one-line-input grammar the rename and address dialogs use.
    if let Some((editing, input)) = panel.and_then(|panel| panel.editing())
        && editing == row
    {
        spans.push(Span::styled(
            format!("{input}█"),
            Style::new().fg(palette.fg),
        ));
        return Line::from(spans);
    }

    match pending {
        // An unapplied answer names what it replaces, `old → new`: the
        // pending mark alone said *that* the row would change, but never
        // what it would stop saying --- that took opening the review.
        Some(Some(value)) => {
            let new = choice_label(row, value);
            let old = saved
                .as_deref()
                .map(|value| choice_label(row, value))
                .or_else(|| app.project_config_fallback(row).map(|(value, _)| value));
            match old {
                Some(old) => {
                    let old = super::overlay::shorten_tail(&old, budget / 2);
                    let new = super::overlay::shorten_tail(
                        &new,
                        budget.saturating_sub(old.chars().count() + 3),
                    );
                    spans.push(Span::styled(old, muted_style(palette)));
                    spans.push(Span::styled(" → ", muted_style(palette)));
                    spans.push(Span::styled(
                        new,
                        Style::new()
                            .fg(palette.warning)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                None => spans.push(Span::styled(
                    super::overlay::shorten_tail(&new, budget),
                    Style::new()
                        .fg(palette.warning)
                        .add_modifier(Modifier::BOLD),
                )),
            }
        }
        Some(None) => {
            let old = saved
                .as_deref()
                .map(|value| choice_label(row, value))
                .or_else(|| app.project_config_fallback(row).map(|(value, _)| value));
            if let Some(old) = old {
                spans.push(Span::styled(
                    super::overlay::shorten_tail(&old, budget.saturating_sub(13)),
                    muted_style(palette),
                ));
                spans.push(Span::styled(" → ", muted_style(palette)));
            }
            spans.push(Span::styled(
                "(removed)",
                Style::new()
                    .fg(palette.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        None => match saved {
            Some(value) => spans.push(Span::styled(
                super::overlay::shorten_tail(&choice_label(row, &value), budget),
                Style::new().fg(palette.success),
            )),
            None => match app.project_config_fallback(row) {
                Some((value, _)) => {
                    spans.push(Span::styled(
                        super::overlay::shorten_tail(&value, budget),
                        muted_style(palette),
                    ));
                }
                None => spans.push(Span::styled("—", muted_style(palette))),
            },
        },
    }
    Line::from(spans)
}

fn display_label(panel: &ProjectConfigPanel, row: ProjectConfigRow) -> String {
    if row != ProjectConfigRow::OtaAddress {
        return row.label().to_string();
    }
    match panel.ota_transport() {
        crate::ota::Transport::Udp => "Board IP address".to_string(),
        crate::ota::Transport::Serial => "Serial port".to_string(),
        crate::ota::Transport::Ble => "Bluetooth address".to_string(),
    }
}

fn display_hint(panel: &ProjectConfigPanel, row: ProjectConfigRow) -> &'static str {
    if row != ProjectConfigRow::OtaAddress {
        return row.hint();
    }
    match panel.ota_transport() {
        crate::ota::Transport::Udp => "The board's IPv4 address for UDP updates.",
        crate::ota::Transport::Serial => "The serial port smpmgr uses to reach the board.",
        crate::ota::Transport::Ble => "The Bluetooth address smpmgr uses to reach the board.",
    }
}

/// The details column: a bordered pane of labelled blocks in a fixed
/// order --- what the row is for (with the key's literal spelling, which
/// the list's prose labels no longer carry), what it can be, which level
/// of the configuration stack answers it *now*, and the line an unapplied
/// answer writes. Fixed order is the point: a column that reshuffles per
/// row is a column the eye has to re-learn per row.
fn draw_details(frame: &mut Frame, areas: &ProjectConfigAreas, app: &mut App, palette: Palette) {
    let Some(panel) = app.project_config.as_ref() else {
        return;
    };
    // A pane of its own, with the keyboard's whereabouts on its border:
    // muted while the list drives, accent once `Tab` hands the pane the
    // arrows (the docs pickers' grammar).
    let focused = panel.details_focus() == crate::app::DocsFocus::Details;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(super::border_style(focused, palette))
        .title(Span::styled(" Details ", muted_style(palette)));
    let inner = block.inner(areas.details);
    frame.render_widget(block, areas.details);
    let area = Rect {
        x: inner.x + 1,
        // Right padding plus the scrollbar's column, always reserved, so
        // the text never reflows when a bar appears (the file lists' rule).
        width: inner.width.saturating_sub(3),
        ..inner
    };
    if area.width < 12 || area.height == 0 {
        return;
    }

    let subheading = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            muted_style(palette).add_modifier(Modifier::BOLD),
        ))
    };

    let Some(row) = panel.selected() else {
        // On the cards: the details explain the choice rather than going
        // blank, since that is the one moment the window has nothing else
        // to say.
        let kind = panel.chosen();
        let mut lines = vec![
            Line::from(Span::styled(
                "Backend",
                Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled("project_type", muted_style(palette))),
            Line::from(""),
            Line::from(Span::styled(
                "What this directory is. It outranks detection and the project registry, and it \
                 travels with the project — commit it and the team shares the answer.",
                Style::new().fg(palette.fg),
            )),
        ];
        if let Some(kind) = kind {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("project_type = \"{}\"", kind.id()),
                Style::new().fg(if panel.backend_changed() {
                    palette.warning
                } else {
                    palette.muted
                }),
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    };

    let mut lines = vec![
        Line::from(Span::styled(
            display_label(panel, row),
            Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            match row.slot() {
                Some((section, key)) if !section.is_empty() => format!("[{section}] {key}"),
                Some((_, key)) => format!("top level · {key}"),
                None => match row.destination() {
                    Destination::Registry => "project registry entry".to_string(),
                    Destination::ReadOnly => "read only".to_string(),
                    _ => String::new(),
                },
            },
            muted_style(palette),
        )),
        Line::from(""),
    ];
    // The hint is pre-wrapped (the docs pickers' own rule) rather than left
    // to `Paragraph::wrap`, so the pane's line count is exact and the
    // scroll offset clamps against a length that is really what is drawn.
    for part in super::overlay::wrap_words(display_hint(panel, row), area.width as usize) {
        lines.push(Line::from(Span::styled(part, Style::new().fg(palette.fg))));
    }

    if let RowKind::Choice(ids) = row.kind() {
        lines.push(Line::from(""));
        lines.push(subheading("Options"));
        let current = panel.value(row);
        // The complete list, always --- the pane scrolls (`Tab`, then the
        // arrows), so even the thirty-odd themes are all here to be read,
        // not summarised.
        for id in &ids {
            let on = current.as_deref() == Some(id.as_str());
            lines.push(Line::from(vec![
                Span::styled(
                    if on { "● " } else { "○ " },
                    Style::new().fg(if on { palette.success } else { palette.muted }),
                ),
                Span::styled(
                    choice_label(row, id),
                    if on {
                        Style::new().fg(palette.fg)
                    } else {
                        muted_style(palette)
                    },
                ),
            ]));
        }
    }

    // The build variants: a report row, so in place of the "Current"
    // levels it gets the session's resolved list --- one entry per
    // variant, the one being built marked, every origin named. The
    // session's answer is the truth, but a session that has not resolved
    // any yet (no build panel before the first scan) falls back to what
    // the file declares, read live --- the row exists for the discovery's
    // answer as much as for the declaration.
    if row == ProjectConfigRow::Variants {
        lines.push(Line::from(""));
        lines.push(subheading("Session"));
        let declared;
        let (variants, selected) = match app.build.as_ref() {
            Some(build) if !build.variants.is_empty() => {
                (build.variants.as_slice(), build.variant_name())
            }
            _ => {
                declared = panel.variant_list();
                (declared.as_slice(), None)
            }
        };
        let entries = crate::project_config::variant_entries(variants, selected);
        for entry in &entries {
            lines.push(Line::from(vec![
                Span::styled(
                    if entry.active { "▸ " } else { "○ " },
                    Style::new().fg(if entry.active {
                        palette.accent
                    } else {
                        palette.muted
                    }),
                ),
                Span::styled(
                    format!("{:<14}", entry.name),
                    if entry.active {
                        Style::new().fg(palette.fg)
                    } else {
                        muted_style(palette)
                    },
                ),
                Span::styled(entry.origin, muted_style(palette)),
            ]));
            lines.push(Line::from(Span::styled(
                format!(
                    "    {}",
                    super::overlay::shorten_tail(
                        &entry.detail,
                        area.width.saturating_sub(4) as usize
                    )
                ),
                muted_style(palette),
            )));
        }
        if entries.is_empty() {
            lines.push(Line::from(Span::styled("—", muted_style(palette))));
        }
    }

    // The levels that can answer this row, most specific first, with the
    // one currently answering marked. An absent key is a question answered
    // somewhere less specific, not an open one --- this block is where the
    // window says so.
    if row != ProjectConfigRow::Variants {
        lines.push(Line::from(""));
        lines.push(subheading("Current"));
        let saved = panel.saved(row);
        let destination = match row.destination() {
            Destination::User => "user config",
            Destination::Registry => "project registry",
            _ => "chiptui.toml",
        };
        let fallback = app.project_config_fallback(row);
        // A level's value is one line, cut from the left when long --- a path's
        // tail is the identifying half, the same call the list rows make.
        let value_budget = area.width.saturating_sub(22) as usize;
        lines.push(level(
            destination,
            saved
                .clone()
                .map(|value| choice_label(row, &value))
                .map(|value| super::overlay::shorten_tail(&value, value_budget)),
            saved.is_some(),
            palette,
        ));
        if saved.is_none()
            && let Some((value, origin)) = fallback
        {
            lines.push(level(
                origin,
                Some(super::overlay::shorten_tail(&value, value_budget)),
                true,
                palette,
            ));
        }

        if let Some(pending) = panel.pending_for(row) {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Will write",
                Style::new()
                    .fg(palette.warning)
                    .add_modifier(Modifier::BOLD),
            )));
            if let Some((section, _)) = row.slot()
                && !section.is_empty()
            {
                lines.push(Line::from(Span::styled(
                    format!("[{section}]"),
                    muted_style(palette),
                )));
            }
            let change = crate::project_config::Pending {
                row,
                value: pending.map(str::to_string),
            };
            lines.push(Line::from(Span::styled(
                super::overlay::shorten_tail(&change.line(), area.width as usize),
                Style::new().fg(palette.warning),
            )));
            lines.push(Line::from(Span::styled(
                format!("in the {}", row.destination().label()),
                muted_style(palette),
            )));
        }
    }

    // The pane scrolls over the rows actually drawn: the viewport is
    // published for the key handler's paging, and the offset is clamped
    // against the exact length (every line was pre-wrapped or shortened
    // above, the docs pickers' contract).
    let scroll = panel.details_scroll();
    let viewport = area.height as usize;
    let total = lines.len();
    let scroll = scroll.min(total.saturating_sub(viewport));
    app.config_details_viewport = viewport;
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), area);
    draw_scrollbar(frame, inner, total, viewport, scroll, palette);
    if total > viewport {
        // The border's bottom rule carries the way to the rest of the
        // list --- the pickers' hint line, riding chrome instead of
        // spending a content row on it.
        let hint = if focused {
            " ↑ ↓  scroll · tab  list "
        } else {
            " tab  reads on "
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, muted_style(palette))))
                .alignment(ratatui::layout::Alignment::Right),
            Rect {
                x: inner.x,
                y: areas.details.y + areas.details.height - 1,
                width: inner.width,
                height: 1,
            },
        );
    }
}

/// One line of the "Current" block: a level of the configuration stack,
/// what it answers for this row (`—` when silent), and `▸` on the level
/// whose answer is in effect.
fn level(name: &str, value: Option<String>, winner: bool, palette: Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if winner { "▸ " } else { "  " },
            Style::new().fg(palette.accent),
        ),
        Span::styled(format!("{name:<18}"), muted_style(palette)),
        match value {
            Some(value) => Span::styled(
                value,
                if winner {
                    Style::new().fg(palette.fg)
                } else {
                    muted_style(palette)
                },
            ),
            None => Span::styled("—", muted_style(palette)),
        },
    ])
}

/// The pending count on the left, the keys that matter on the right.
fn draw_footer(frame: &mut Frame, areas: &ProjectConfigAreas, app: &App, palette: Palette) {
    let Some(panel) = app.project_config.as_ref() else {
        return;
    };
    let count = panel.change_count();
    let left = match count {
        0 => Span::styled("No changes yet", muted_style(palette)),
        1 => Span::styled(
            "1 change waiting",
            Style::new()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ),
        many => Span::styled(
            format!("{many} changes waiting"),
            Style::new()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ),
    };

    let keys = if panel.editing().is_some() {
        "enter  save     del  clear the field     esc  cancel".to_string()
    } else {
        let row = match panel.selected().map(ProjectConfigRow::kind) {
            None => "← →  pick     enter  settings     ",
            Some(RowKind::Choice(_)) => "← →  change     del  clear     ",
            Some(RowKind::Text)
                if panel
                    .selected()
                    .is_some_and(|row| row.picker_kind().is_some() || row.uses_target_picker()) =>
            {
                "enter  browse     ctrl+e  type     del  clear     "
            }
            Some(RowKind::Text) => "enter  edit     del  clear     ",
            _ => "",
        };
        let close = if count > 0 {
            "ctrl+s  apply     esc  leave…"
        } else {
            "esc  close"
        };
        format!("{row}{close}")
    };

    let width = areas.footer.width as usize;
    let used = left.content.chars().count() + keys.chars().count();
    let gap = width.saturating_sub(used).max(2);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            left,
            Span::raw(" ".repeat(gap)),
            Span::styled(keys, muted_style(palette)),
        ])),
        areas.footer,
    );
}
