//! `esptool` flash/erase view rendering: menu, options and online-firmware
//! screens. Streamed command output no longer lives here --- it renders in
//! the dashboard's Monitor tab (`crate::ui::monitor`) once a command starts,
//! so the dialog itself closes instead of growing an "output screen".
//!
//! Kept apart from the file browser pane, matching `SPEC.md` §9's requirement
//! that esptool operations are presented separately from the filesystem.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::App;
use crate::flash::{ChipGuess, FlashAction, FlashPanel, FlashScreen, OptionsField};
use crate::ui::{Palette, SPINNER, muted_style, pane_block, selection_style};

/// The dialog's width×height for `flash.screen`, sized to its content like
/// every other modal in `ui::overlay` rather than a fraction of the screen.
pub fn dialog_size(flash: &FlashPanel) -> (u16, u16) {
    match flash.screen {
        FlashScreen::Menu => (60, FlashAction::ALL.len() as u16 + 2),
        FlashScreen::Options => {
            let fields = FlashPanel::options_fields(flash.selected_action()).len() as u16;
            let firmware_line = u16::from(flash.selected_firmware_path().is_some());
            (66, fields + firmware_line + 4)
        }
        // The online screens carry a source line and a local-folder note
        // around the list itself (`+ 4` content rows).
        FlashScreen::OnlineBoards => (72, flash.online_boards.len().max(2) as u16 + 6),
        FlashScreen::OnlineFirmware => (72, flash.online_firmware.len().max(2) as u16 + 6),
        FlashScreen::CustomUrl => (70, 6),
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let Some(flash) = &app.flash else {
        frame.render_widget(
            Paragraph::new("the flash view has not been opened".fg(palette.muted)),
            area,
        );
        return;
    };

    let focused = app.overlay.is_none();
    let ticks = app.ticks;
    match flash.screen {
        FlashScreen::Menu => draw_menu(frame, area, flash, focused, palette),
        FlashScreen::Options => draw_options(frame, area, flash, focused, palette),
        FlashScreen::OnlineBoards => {
            draw_online_boards(frame, area, flash, ticks, focused, palette)
        }
        FlashScreen::OnlineFirmware => {
            draw_online_firmware(frame, area, flash, ticks, focused, palette)
        }
        FlashScreen::CustomUrl => draw_custom_url(frame, area, flash, focused, palette),
    }
}

fn draw_menu(frame: &mut Frame, area: Rect, flash: &FlashPanel, focused: bool, palette: Palette) {
    let items: Vec<ListItem> = FlashAction::ALL
        .iter()
        .map(|action| {
            let mut spans = vec![Span::styled(
                format!(" {} {} ", action.icon(), action.label()),
                Style::new().fg(palette.fg),
            )];
            // Destructive operations are flagged wherever they appear, same
            // convention as the capabilities pane (`SPEC.md` §15).
            if action.is_destructive() {
                spans.push(Span::styled("confirm", Style::new().fg(palette.warning)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(flash.cursor));
    frame.render_stateful_widget(
        List::new(items)
            .block(pane_block("Flash", focused, palette))
            .highlight_style(selection_style(palette)),
        area,
        &mut state,
    );
}

fn draw_options(
    frame: &mut Frame,
    area: Rect,
    flash: &FlashPanel,
    focused: bool,
    palette: Palette,
) {
    let action = flash.selected_action();
    let fields = FlashPanel::options_fields(action);

    let mut lines = vec![
        Line::from(action.label().fg(palette.fg).bold()),
        Line::from(""),
    ];

    for field in fields {
        let label = field_label(*field);
        let value = field_value(flash, *field);
        let value_style = if flash.options_focus == *field {
            Style::new().fg(palette.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(palette.fg)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<12}"), muted_style(palette)),
            Span::styled(value, value_style),
        ]));
    }

    if let Some(firmware) = flash.selected_firmware_path() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(format!("{:<12}", "firmware"), muted_style(palette)),
            Span::styled(firmware.display().to_string(), Style::new().fg(palette.fg)),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(pane_block("Options", focused, palette))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Shared with `crate::ui::monitor`'s flash-output recap.
pub(super) fn field_label(field: OptionsField) -> &'static str {
    match field {
        OptionsField::Chip => "chip",
        OptionsField::Offset => "offset",
        OptionsField::FlashMode => "flash mode",
        OptionsField::FlashFreq => "flash freq",
        OptionsField::FlashSize => "flash size",
        OptionsField::ExtraArgs => "extra flags",
    }
}

/// Shared with `crate::ui::monitor`'s flash-output recap.
pub(super) fn field_value(flash: &FlashPanel, field: OptionsField) -> String {
    match field {
        OptionsField::Chip => match flash.chip {
            ChipGuess::Unknown => "auto".to_string(),
            ChipGuess::Detected(family) => format!("{} (detected)", family.label()),
            ChipGuess::Overridden(family) => family.label().to_string(),
        },
        OptionsField::Offset => flash.offset.clone(),
        OptionsField::FlashMode => flash.options.flash_mode.map_or_else(
            || "(unset)".to_string(),
            |mode| mode.esptool_id().to_string(),
        ),
        OptionsField::FlashFreq => flash.options.flash_freq.map_or_else(
            || "(unset)".to_string(),
            |freq| freq.esptool_id().to_string(),
        ),
        OptionsField::FlashSize => flash.options.flash_size.map_or_else(
            || "(unset)".to_string(),
            |size| size.esptool_id().to_string(),
        ),
        OptionsField::ExtraArgs => flash.options.extra_args.clone(),
    }
}

/// The shared frame of the two online screens: the source the list is being
/// fetched from (or was), a live status line, the list itself, and the note
/// that keeps the local `firmware/` folder's priority explicit --- the online
/// search is the fallback, never the silent winner over a local image
/// (`SPEC.md` §9).
struct OnlineFrame {
    /// Spinner phase for the fetching status line ([`crate::ui::SPINNER`]).
    ticks: u64,
    title: &'static str,
    /// The status line under the source while the list is being fetched.
    fetching: Option<&'static str>,
    /// The status line once the list has content.
    settled: String,
    /// The status line when the fetch finished and nothing arrived.
    empty: &'static str,
    /// The list rows; empty means there is nothing to pick (yet).
    items: Vec<ListItem<'static>>,
}

/// The last path segment of [`FlashPanel::firmware_dir`] — enough to name
/// the folder inside the dialog; the full path is in the log notices.
fn firmware_dir_name(flash: &FlashPanel) -> String {
    flash.firmware_dir.file_name().map_or_else(
        || flash.firmware_dir.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn draw_online_frame(
    frame: &mut Frame,
    area: Rect,
    flash: &FlashPanel,
    focused: bool,
    palette: Palette,
    online: OnlineFrame,
) {
    let block = pane_block(online.title, focused, palette);
    let inner = block.inner(area);
    let [head, list_area, foot] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(inner);

    let source = Line::from(vec![
        Span::styled("source  ", muted_style(palette)),
        Span::styled(
            flash
                .online_source
                .as_deref()
                .unwrap_or("micropython.org/download")
                .trim_start_matches("https://"),
            Style::new().fg(palette.fg),
        ),
    ]);
    let status = match online.fetching {
        Some(what) => Line::from(vec![
            Span::styled(
                SPINNER[(online.ticks as usize) % SPINNER.len()],
                Style::new().fg(palette.accent),
            ),
            Span::styled(format!(" {what}"), Style::new().fg(palette.accent)),
        ]),
        None if online.items.is_empty() => Line::from(vec![
            Span::styled(online.empty, Style::new().fg(palette.warning)),
            Span::styled("  (u pastes a direct URL)", muted_style(palette)),
        ]),
        None => Line::from(Span::styled(online.settled, muted_style(palette))),
    };
    frame.render_widget(
        Paragraph::new(vec![source, status]).wrap(Wrap { trim: false }),
        head,
    );

    if !online.items.is_empty() {
        let mut state = ListState::default().with_selected(Some(flash.online_cursor));
        frame.render_stateful_widget(
            List::new(online.items).highlight_style(selection_style(palette)),
            list_area,
            &mut state,
        );
    }

    let dir = firmware_dir_name(flash);
    let hint = if flash.firmware.is_empty() {
        format!("no .bin/.elf in {dir}/ yet — one added there is picked first")
    } else {
        format!(
            "{} local image{} in {dir}/ — local files come first",
            flash.firmware.len(),
            if flash.firmware.len() == 1 { "" } else { "s" }
        )
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(""), Line::from(hint)]).wrap(Wrap { trim: false }),
        foot,
    );
    frame.render_widget(block, area);
}

/// Boards found by [`crate::flash::FlashPanel::search_online`].
fn draw_online_boards(
    frame: &mut Frame,
    area: Rect,
    flash: &FlashPanel,
    ticks: u64,
    focused: bool,
    palette: Palette,
) {
    let online = OnlineFrame {
        ticks,
        title: "Firmware online",
        fetching: flash.searching_boards().then_some("searching for boards…"),
        settled: format!(
            "{} board{} for this query — enter lists its firmware",
            flash.online_boards.len(),
            if flash.online_boards.len() == 1 {
                ""
            } else {
                "s"
            }
        ),
        empty: "no boards found for this chip",
        items: flash
            .online_boards
            .iter()
            .map(|board| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", board.product), Style::new().fg(palette.fg)),
                    Span::styled(
                        format!("{}  ", board.vendor),
                        Style::new().fg(palette.accent),
                    ),
                    Span::styled(board.id.clone(), muted_style(palette)),
                ]))
            })
            .collect(),
    };
    draw_online_frame(frame, area, flash, focused, palette, online)
}

/// Firmware builds found for the board picked from [`draw_online_boards`].
fn draw_online_firmware(
    frame: &mut Frame,
    area: Rect,
    flash: &FlashPanel,
    ticks: u64,
    focused: bool,
    palette: Palette,
) {
    let fetching = if flash.downloading_firmware() {
        Some("downloading…")
    } else if flash.fetching_firmware_list() {
        Some("fetching the board's firmware page…")
    } else {
        None
    };
    let online = OnlineFrame {
        ticks,
        title: "Firmware online",
        fetching,
        settled: format!(
            "{} build{} — enter downloads it into {}",
            flash.online_firmware.len(),
            if flash.online_firmware.len() == 1 {
                ""
            } else {
                "s"
            },
            firmware_dir_name(flash)
        ),
        empty: "no flashable .bin firmware for this board",
        items: flash
            .online_firmware
            .iter()
            .map(|file| {
                let mut spans = vec![Span::styled(
                    format!(" {} ", file.label),
                    Style::new().fg(palette.fg),
                )];
                if !file.variant.is_empty() {
                    spans.push(Span::styled(
                        format!("{}  ", file.variant),
                        muted_style(palette),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect(),
    };
    draw_online_frame(frame, area, flash, focused, palette, online)
}

/// Free-text entry for a direct firmware download URL (`SPEC.md` §9: the
/// user may paste a link instead of searching).
fn draw_custom_url(
    frame: &mut Frame,
    area: Rect,
    flash: &FlashPanel,
    focused: bool,
    palette: Palette,
) {
    let lines = vec![
        Line::from("Paste a direct firmware download URL, then press enter.".fg(palette.muted)),
        Line::from(""),
        Line::from(vec![
            Span::styled("url  ", muted_style(palette)),
            Span::styled(
                flash.custom_url.as_str(),
                Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .block(pane_block("Firmware URL", focused, palette))
            .wrap(Wrap { trim: false }),
        area,
    );
}
