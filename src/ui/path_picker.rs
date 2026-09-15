//! One visual/geometry contract for every local path picker, including Home.
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::{Palette, icon_column, selection_style};
use crate::icons::IconSet;
use crate::path_picker::{EntryKind, PathPicker, PickerFocus, PickerKind};

pub const WIDTH: u16 = 78;
pub const HEIGHT: u16 = 22;

/// Keep the cursor/path tail visible in terminal cells, including wide
/// Unicode names, without splitting a UTF-8 character.
fn visible_tail(text: &str, width: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        let cells = Line::raw(ch.to_string()).width();
        if used + cells > width {
            break;
        }
        used += cells;
        start = index;
    }
    &text[start..]
}

pub fn areas(popup: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .areas(popup.inner(ratatui::layout::Margin::new(1, 1)))
}

pub fn click(picker: &mut PathPicker, popup: Rect, point: (u16, u16)) {
    if picker.help {
        return;
    }
    let [path, _, list, _] = areas(popup);
    let inside = |rect: Rect| rect.contains(ratatui::layout::Position::new(point.0, point.1));
    if inside(path) {
        picker.edit_path();
    } else if inside(list) && list.height > 0 {
        let offset = picker.selected.saturating_sub(list.height as usize - 1);
        let index = offset + (point.1 - list.y) as usize;
        if index < picker.entries.len() {
            picker.focus = PickerFocus::List;
            picker.selected = index;
        }
    }
}

pub fn draw(
    frame: &mut Frame,
    popup: Rect,
    title: &str,
    picker: &PathPicker,
    palette: Palette,
    icons: IconSet,
) {
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(palette.accent))
            .title(format!(" {title} ")),
        popup,
    );
    let [path, summary, list, footer] = areas(popup);
    let editing = picker.focus != PickerFocus::List;
    let label = if picker.focus == PickerFocus::NewDirectory {
        "new  "
    } else {
        "path "
    };
    let text = if editing {
        let before = visible_tail(
            &picker.input[..picker.cursor],
            path.width.saturating_sub(7) as usize,
        );
        format!("{before}▏{}", &picker.input[picker.cursor..])
    } else {
        visible_tail(
            &picker.path.to_string_lossy(),
            path.width.saturating_sub(5) as usize,
        )
        .to_string()
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(label, Style::new().fg(palette.muted)),
            Span::styled(
                text,
                Style::new().fg(if editing { palette.accent } else { palette.fg }),
            ),
        ])),
        path,
    );
    let filter = match picker.kind {
        PickerKind::Directory => "directories",
        PickerKind::File(filter) => filter.label(),
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{filter} · hidden {}",
            if picker.show_hidden {
                "shown"
            } else {
                "hidden"
            }
        ))
        .fg(palette.muted),
        summary,
    );
    if picker.help {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("↑/↓, j/k: move · PgUp/PgDn: page · Home/End: first/last"),
                Line::from("Enter: open folder / select file / use current directory"),
                Line::from("→: open folder · ←/Backspace: parent"),
                Line::from("Tab or Ctrl+L: edit path (absolute, relative or ~/...)"),
                Line::from("In path: Enter navigates or selects a file; Esc cancels edit"),
                Line::from("Ctrl+U: clear input · ←/→, Home/End: edit cursor"),
                Line::from(". (list): hidden entries · Ctrl+R: refresh"),
                Line::from("Ctrl+N: create a directory (directory picker only)"),
                Line::from("Esc: cancel picker · F1: close this help"),
            ])
            .fg(palette.fg)
            .wrap(Wrap { trim: false }),
            list,
        );
    } else {
        let items: Vec<ListItem> = picker
            .entries
            .iter()
            .map(|entry| ListItem::new(row_text(entry, icons)))
            .collect();
        let mut state = ListState::default().with_selected(Some(picker.selected));
        frame.render_stateful_widget(
            List::new(items).fg(palette.fg).highlight_style(if editing {
                Style::new().fg(palette.muted)
            } else {
                selection_style(palette)
            }),
            list,
            &mut state,
        );
        if !picker
            .entries
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Directory | EntryKind::File))
            && picker.error.is_none()
            && list.height > 2
        {
            frame.render_widget(
                Paragraph::new("No matching entries in this folder.").fg(palette.muted),
                Rect::new(list.x, list.y + 2, list.width, 1),
            );
        }
    }
    let hints = if picker.kind == PickerKind::Directory {
        "ctrl+l path · . hidden · ctrl+n new folder · F1 help"
    } else {
        "ctrl+l path · . hidden · ctrl+r refresh · F1 help"
    };
    let lines = vec![
        Line::from(hints.fg(palette.muted)),
        Line::from(picker.error.as_deref().unwrap_or("").fg(palette.error)),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), footer);
}

/// One listing row's text: the kind glyph plus the name. The glyph rides
/// the same three-cell decoration column the file browser budgets
/// ([`super::icon_column`]) when `[ui] icons` shows decorations; the
/// `none` set drops the column whole and every row reads exactly as it
/// did before the picker had glyphs. Directories keep their trailing
/// `/` --- that mark is information, not decoration.
fn row_text(entry: &crate::path_picker::Entry, icons: IconSet) -> String {
    let name = match entry.kind {
        EntryKind::Directory => format!("{}/", entry.name),
        _ => entry.name.clone(),
    };
    if !icons.shows_decorations() {
        return match entry.kind {
            EntryKind::Use => format!("→ {name}"),
            _ => format!("  {name}"),
        };
    }
    let nerd = matches!(icons, IconSet::Nerd);
    let (glyph, single_cell) = match entry.kind {
        // The accept row's arrow is its label's own, not a kind mark ---
        // but it still rides the column so the names below it line up.
        EntryKind::Use => ("→", true),
        EntryKind::Parent => (icons.folder_open(), nerd),
        EntryKind::Directory => (icons.directory(), nerd),
        EntryKind::File => (icons.file(), nerd),
    };
    format!("{}{name}", icon_column(glyph, single_cell))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_picker::{Entry, FileFilter};
    use std::path::{Path, PathBuf};

    #[test]
    fn resized_draws_and_clicks_share_the_cached_listing_and_cursor() {
        let mut picker = PathPicker::new(
            PickerKind::File(FileFilter::All),
            PathBuf::from("/missing-picker-fixture"),
            Path::new("/"),
        );
        picker.error = None;
        picker.entries = (0..45)
            .map(|index| Entry {
                name: format!("file-{index:02}.bin"),
                path: PathBuf::from(format!("/file-{index:02}.bin")),
                kind: EntryKind::File,
            })
            .collect();
        let palette = ratatui_themes::ThemeName::TokyoNight.palette();
        for (width, height) in [(100, 40), (80, 32), (38, 14)] {
            picker.selected = 40;
            let popup = super::super::centered(Rect::new(0, 0, width, height), WIDTH, HEIGHT);
            let list = areas(popup)[2];
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        popup,
                        "Choose a file",
                        &picker,
                        palette,
                        IconSet::None,
                    )
                })
                .unwrap();
            let index = 40usize.saturating_sub(list.height as usize - 1);
            let row = (list.x..list.right())
                .map(|x| terminal.backend().buffer()[(x, list.y)].symbol())
                .collect::<String>();
            assert!(row.contains(&format!("file-{index:02}.bin")), "{row}");
            click(&mut picker, popup, (list.x + 1, list.y));
            assert_eq!(picker.selected, index);
        }
        for (width, height) in [(1, 1), (12, 4)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        frame.area(),
                        "Choose a file",
                        &picker,
                        palette,
                        IconSet::None,
                    )
                })
                .unwrap();
        }
        assert_eq!(visible_tail("abc界界", 4), "界界");
        assert_eq!(visible_tail("abc界界", 3), "界");
    }

    /// The kind glyph answers `[ui] icons`: two-cell emoji in the Unicode
    /// set, the single-width Nerd marks centered into the same three-cell
    /// column, and no column at all under `none` (the rows then read
    /// exactly as they did before the picker had glyphs).
    #[test]
    fn row_text_follows_the_icon_set() {
        let entry = |name: &str, kind: EntryKind| Entry {
            name: name.to_string(),
            path: PathBuf::from(name),
            kind,
        };
        let dir = entry("sub", EntryKind::Directory);
        let file = entry("app.bin", EntryKind::File);
        let parent = entry("..", EntryKind::Parent);
        let accept = entry("use this directory", EntryKind::Use);

        assert_eq!(
            row_text(&dir, IconSet::Unicode),
            "📁 sub/",
            "directories keep their trailing slash after the glyph"
        );
        assert_eq!(row_text(&file, IconSet::Unicode), "📄 app.bin");
        assert_eq!(row_text(&parent, IconSet::Unicode), "📂 ..");
        assert_eq!(
            row_text(&accept, IconSet::Unicode),
            " → use this directory",
            "the accept row's arrow rides the same column"
        );

        assert_eq!(row_text(&dir, IconSet::Nerd), " \u{F07B} sub/");
        assert_eq!(row_text(&file, IconSet::Nerd), " \u{F15B} app.bin");
        assert_eq!(row_text(&parent, IconSet::Nerd), " \u{F07C} ..");

        assert_eq!(row_text(&dir, IconSet::None), "  sub/");
        assert_eq!(row_text(&file, IconSet::None), "  app.bin");
        assert_eq!(row_text(&parent, IconSet::None), "  ..");
        assert_eq!(row_text(&accept, IconSet::None), "→ use this directory");
    }
}
