//! `esptool` flash/erase view rendering: menu, options and online-firmware
//! screens. Streamed command output no longer lives here --- it renders in
//! the dashboard's Monitor tab (`crate::ui::monitor`) once a command starts,
//! so the dialog itself closes instead of growing an "output screen".
//!
//! Kept apart from the file browser pane, matching `SPEC.md` §9's requirement
//! that esptool operations are presented separately from the filesystem.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::App;
use crate::flash::{ChipGuess, FlashAction, FlashPanel, FlashScreen, OptionsField};
use crate::ui::{Palette, pane_block};

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
        FlashScreen::OnlineBoards => (64, flash.online_boards.len().max(2) as u16 + 2),
        FlashScreen::OnlineFirmware => (64, flash.online_firmware.len().max(2) as u16 + 2),
        FlashScreen::CustomUrl => (70, 6),
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let Some(flash) = &app.flash else {
        frame.render_widget(
            Paragraph::new("the flash view has not been opened".dim()),
            area,
        );
        return;
    };

    let focused = app.overlay.is_none();
    match flash.screen {
        FlashScreen::Menu => draw_menu(frame, area, flash, focused, palette),
        FlashScreen::Options => draw_options(frame, area, flash, focused, palette),
        FlashScreen::OnlineBoards => draw_online_boards(frame, area, flash, focused, palette),
        FlashScreen::OnlineFirmware => draw_online_firmware(frame, area, flash, focused, palette),
        FlashScreen::CustomUrl => draw_custom_url(frame, area, flash, focused, palette),
    }
}

fn draw_menu(frame: &mut Frame, area: Rect, flash: &FlashPanel, focused: bool, palette: Palette) {
    let items: Vec<ListItem> = FlashAction::ALL
        .iter()
        .map(|action| {
            let mut spans = vec![Span::raw(format!(" {} {} ", action.icon(), action.label()))];
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
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
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

    let mut lines = vec![Line::from(action.label().bold()), Line::from("")];

    for field in fields {
        let label = field_label(*field);
        let value = field_value(flash, *field);
        let value_style = if flash.options_focus == *field {
            Style::new().fg(palette.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<12}"), Style::new().dim()),
            Span::styled(value, value_style),
        ]));
    }

    if let Some(firmware) = flash.selected_firmware_path() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(format!("{:<12}", "firmware"), Style::new().dim()),
            Span::raw(firmware.display().to_string()),
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

/// Boards found by [`crate::flash::FlashPanel::search_online`].
fn draw_online_boards(
    frame: &mut Frame,
    area: Rect,
    flash: &FlashPanel,
    focused: bool,
    palette: Palette,
) {
    if flash.online_boards.is_empty() {
        frame.render_widget(
            Paragraph::new("no boards found".dim()).block(pane_block(
                "Boards online",
                focused,
                palette,
            )),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = flash
        .online_boards
        .iter()
        .map(|board| {
            ListItem::new(Line::from(vec![
                Span::raw(format!(" {} ", board.product)),
                Span::styled(
                    format!("{}  ", board.vendor),
                    Style::new().fg(palette.accent),
                ),
                Span::styled(board.id.clone(), Style::new().dim()),
            ]))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(flash.online_cursor));
    frame.render_stateful_widget(
        List::new(items)
            .block(pane_block("Boards online", focused, palette))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

/// Firmware builds found for the board picked from [`draw_online_boards`].
fn draw_online_firmware(
    frame: &mut Frame,
    area: Rect,
    flash: &FlashPanel,
    focused: bool,
    palette: Palette,
) {
    if flash.online_firmware.is_empty() {
        frame.render_widget(
            Paragraph::new("no flashable firmware found".dim()).block(pane_block(
                "Firmware online",
                focused,
                palette,
            )),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = flash
        .online_firmware
        .iter()
        .map(|file| {
            let mut spans = vec![Span::raw(format!(" {} ", file.label))];
            if !file.variant.is_empty() {
                spans.push(Span::styled(
                    format!("{}  ", file.variant),
                    Style::new().dim(),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(flash.online_cursor));
    frame.render_stateful_widget(
        List::new(items)
            .block(pane_block("Firmware online", focused, palette))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
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
        Line::from("Paste a direct firmware download URL, then press enter.".dim()),
        Line::from(""),
        Line::from(vec![
            Span::styled("url  ", Style::new().dim()),
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
