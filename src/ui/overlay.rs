//! Modal layers: help and manual backend selection.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, FileAction, Overlay, PickerOption, View, ViewerState};
use crate::backend::BackendKind;
use crate::highlight::{self, TokenKind};

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(overlay) = app.overlay.clone() else {
        return;
    };
    match overlay {
        Overlay::Help => draw_help(frame, area, app),
        Overlay::BackendPicker { selected } => draw_picker(frame, area, app, selected),
        Overlay::DevicePicker { selected } => draw_device_picker(frame, area, app, selected),
        Overlay::Confirm { message } => draw_confirm(frame, area, &message),
        Overlay::FirmwarePicker { selected } => draw_firmware_picker(frame, area, app, selected),
        Overlay::ProjectSetup { selected } => draw_project_setup(frame, area, selected),
        Overlay::ConfirmDownloadOverwrite { url, dest } => {
            draw_confirm_download_overwrite(frame, area, &url, &dest)
        }
        Overlay::FileActions {
            side,
            name,
            selected,
        } => draw_file_actions(frame, area, side, &name, selected),
        Overlay::FileViewer => draw_file_viewer(frame, area, app),
        Overlay::ConfirmRestartDevice { confirm } => {
            draw_confirm_restart_device(frame, area, app, confirm)
        }
    }
}

/// Offered once an edited device file has been re-uploaded. Unlike every
/// other confirm overlay here (a plain `y/enter` vs. `n/esc` line), this one
/// gets a btop-style pair of button widgets --- `confirm` is which one is
/// highlighted, defaulting to "No" so a reflex `Enter` cannot restart the
/// board.
fn draw_confirm_restart_device(frame: &mut Frame, area: Rect, app: &App, confirm: bool) {
    let command = crate::backend::micropython::commands::soft_reset(app.devices.selected_port());

    let popup = centered(area, 54.min(area.width), 9);
    let block = modal("Restart device?");
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [message_area, buttons_area] =
        Layout::vertical([Constraint::Length(4), Constraint::Length(3)]).areas(inner);

    let message = vec![
        Line::from("The edited file was uploaded to the device.".fg(Color::Yellow)),
        Line::from("Restart it now?".fg(Color::Yellow)),
        Line::from(""),
        Line::from(command.to_string().dim()),
    ];
    frame.render_widget(
        Paragraph::new(message).alignment(Alignment::Center),
        message_area,
    );

    let [no_area, _gap, yes_area] = Layout::horizontal([
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Length(10),
    ])
    .flex(Flex::Center)
    .areas(buttons_area);

    draw_dialog_button(frame, no_area, "No", !confirm);
    draw_dialog_button(frame, yes_area, "Yes", confirm);
}

/// One btop-style dialog button: a bordered box, filled solid when selected
/// so the highlighted choice reads at a glance rather than needing the
/// border colour alone to carry it (the same "never rely on colour alone"
/// reasoning as the file panes' sync markers, `ui/files.rs`).
fn draw_dialog_button(frame: &mut Frame, area: Rect, label: &str, selected: bool) {
    let (border_style, text_style) = if selected {
        (
            Style::new().fg(Color::Cyan),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (Style::new().dim(), Style::new().dim())
    };

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(label).alignment(Alignment::Center)).style(text_style),
        inner,
    );
}

/// The three actions for the file `enter` was pressed on (`FileAction::for_side`
/// decides which three), sized like the other small pickers in this module.
fn draw_file_actions(
    frame: &mut Frame,
    area: Rect,
    side: crate::browser::Side,
    name: &str,
    selected: usize,
) {
    let actions = FileAction::for_side(side);
    let items: Vec<ListItem> = actions
        .iter()
        .map(|action| ListItem::new(Line::from(format!(" {} ", action.label()))))
        .collect();

    let popup = centered(area, 44, actions.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal(name))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        popup,
        &mut state,
    );
}

/// A file's contents (`View`, from [`Overlay::FileActions`]), sized to most
/// of the body rather than a small dialog like the other overlays --- unlike
/// a picker or a confirmation, this one is meant to be read.
///
/// Publishes [`App::viewer_viewport`] the same way `panels::draw_logs`
/// publishes `log_viewport`, so `PageUp`/`PageDown` scroll by exactly what is
/// on screen.
fn draw_file_viewer(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(viewer) = &app.viewer else { return };

    let width = area.width.saturating_sub(6).max(20);
    let height = area.height.saturating_sub(4).max(6);
    let popup = centered(area, width, height);

    let name = viewer.display_name();
    let title = match &viewer.state {
        ViewerState::Ready { lines } => format!(
            " {name}  ({}/{}) ",
            (viewer.scroll + 1).min(lines.len().max(1)),
            lines.len()
        ),
        ViewerState::Loading | ViewerState::Error(_) => format!(" {name} "),
    };
    let block = modal(&title);
    let inner = block.inner(popup);
    app.viewer_viewport = inner.height.max(1) as usize;

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    match &viewer.state {
        ViewerState::Loading => {
            frame.render_widget(Paragraph::new("loading…".dim()), inner);
        }
        ViewerState::Error(message) => {
            frame.render_widget(
                Paragraph::new(message.clone().fg(Color::Yellow))
                    .wrap(ratatui::widgets::Wrap { trim: false }),
                inner,
            );
        }
        ViewerState::Ready { lines } => {
            let language = highlight::Language::from_filename(&name);
            let rendered: Vec<Line> = lines
                .iter()
                .map(|line| {
                    Line::from(
                        highlight::highlight_line(line, language)
                            .into_iter()
                            .map(|token| Span::styled(token.text, token_style(token.kind)))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            frame.render_widget(
                Paragraph::new(rendered).scroll((viewer.scroll as u16, 0)),
                inner,
            );
        }
    }
}

fn token_style(kind: TokenKind) -> Style {
    match kind {
        TokenKind::Plain => Style::new(),
        TokenKind::Keyword => Style::new().fg(Color::Magenta),
        TokenKind::String => Style::new().fg(Color::Green),
        TokenKind::Comment => Style::new().fg(Color::DarkGray).italic(),
        TokenKind::Number => Style::new().fg(Color::Cyan),
    }
}

/// A firmware download would overwrite a file already in the project root;
/// mirrors [`draw_confirm`]'s "show exactly what is about to happen" rule,
/// applied to a filesystem write rather than a device operation.
fn draw_confirm_download_overwrite(
    frame: &mut Frame,
    area: Rect,
    url: &str,
    dest: &std::path::Path,
) {
    let lines = vec![
        Line::from(format!("{} already exists.", dest.display()).fg(Color::Yellow)),
        Line::from(format!("Overwrite it by downloading {url}?")),
        Line::from(""),
        Line::from(vec![
            Span::styled("y / enter", Style::new().fg(Color::Cyan)),
            Span::raw("  confirm    "),
            Span::styled("n / esc", Style::new().fg(Color::Cyan)),
            Span::raw("  cancel"),
        ]),
    ];
    let popup = centered(area, 70.min(area.width), 6);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(modal("Overwrite firmware?"))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        popup,
    );
}

/// Empty or unrecognized project (`SPEC.md` §7): asks which backend this
/// directory is, offering no "Automatic" row since detection already failed
/// to conclude one.
fn draw_project_setup(frame: &mut Frame, area: Rect, selected: usize) {
    let items: Vec<ListItem> = BackendKind::ALL
        .iter()
        .map(|kind| ListItem::new(Line::from(format!(" {} ", kind.display_name()))))
        .collect();

    let popup = centered(area, 60, BackendKind::ALL.len() as u16 + 4);
    let block = modal("New project");
    let inner = block.inner(popup);
    let [message, list] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(BackendKind::ALL.len() as u16),
    ])
    .areas(inner);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(
            "No known project type here --- pick one; ChipTUI will remember this folder."
                .to_string()
                .dim(),
        )
        .wrap(ratatui::widgets::Wrap { trim: false }),
        message,
    );

    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        list,
        &mut state,
    );
}

/// A destructive esptool action awaiting explicit confirmation (`SPEC.md`
/// §15). `message` is always the literal command about to run, never a
/// paraphrase, so shown as-is.
fn draw_confirm(frame: &mut Frame, area: Rect, message: &str) {
    let lines = vec![
        Line::from(message.to_string().fg(Color::Yellow)),
        Line::from(""),
        Line::from(vec![
            Span::styled("y / enter", Style::new().fg(Color::Cyan)),
            Span::raw("  confirm    "),
            Span::styled("n / esc", Style::new().fg(Color::Cyan)),
            Span::raw("  cancel"),
        ]),
    ];
    let popup = centered(area, 70.min(area.width), 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(modal("Confirm"))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        popup,
    );
}

/// Chooses among several `.bin`/`.elf` candidates found in the project root.
fn draw_firmware_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize) {
    let firmware = app
        .flash
        .as_ref()
        .map(|flash| flash.firmware.as_slice())
        .unwrap_or_default();

    if firmware.is_empty() {
        let popup = centered(area, 52, 4);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![Line::from(
                "No .bin/.elf firmware found in the project root.".fg(Color::Yellow),
            )])
            .block(modal("Firmware")),
            popup,
        );
        return;
    }

    let items: Vec<ListItem> = firmware
        .iter()
        .map(|entry| {
            ListItem::new(Line::from(vec![
                Span::raw(format!(" {} ", entry.name)),
                Span::styled(format!("{} bytes", entry.size), Style::new().dim()),
            ]))
        })
        .collect();

    let popup = centered(area, 64, firmware.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal("Firmware"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        popup,
        &mut state,
    );
}

/// Serial device selection. Reached automatically when a scan finds more than
/// one board, because guessing would be the wrong kind of convenience.
fn draw_device_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize) {
    let devices = app.devices.devices();

    if devices.is_empty() {
        let popup = centered(area, 52, 4);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("No MicroPython device found.".fg(Color::Yellow)),
                Line::from("Connect a board and press 'd' to scan again.".dim()),
            ])
            .block(modal("Device")),
            popup,
        );
        return;
    }

    let items: Vec<ListItem> = devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let mut spans = vec![Span::raw(format!(" {} ", device.label()))];
            if app.devices.selected_index() == Some(index) {
                spans.push(Span::styled("(active)", Style::new().dim()));
            }
            let vid_pid = match device.vendor() {
                Some(vendor) => format!("  {} ({vendor})", device.vid_pid),
                None => format!("  {}", device.vid_pid),
            };
            spans.push(Span::styled(vid_pid, Style::new().fg(Color::Cyan)));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let popup = centered(area, 64, devices.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal("Device"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        popup,
        &mut state,
    );
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    // Help follows the screen: listing dashboard keys while browsing files
    // would describe bindings that do nothing.
    let bindings: &[(&str, &str)] = match app.view {
        View::Dashboard => &[
            ("tab / shift+tab", "move focus between panes"),
            ("↑ ↓ / k j", "navigate inside the focused pane"),
            ("page up/down", "scroll the log by one screen"),
            ("home / end", "jump to start / end"),
            (
                "r",
                "re-run project detection, or reload the focused file browser pane",
            ),
            ("o", "override the detected backend"),
            (
                "enter / →",
                "open a directory, or a text file's menu (send/download, view, edit)",
            ),
            (
                "backspace / ←",
                "go to the parent directory in the file browser",
            ),
            ("c", "compare the selected file by sha256"),
            ("h", "show or hide dot-files in the file browser"),
            ("d", "scan for devices (when the backend has a filesystem)"),
            ("e", "in the file viewer: edit with $EDITOR"),
            ("?", "toggle this help"),
            ("q / esc / ctrl+c", "quit"),
        ],
        View::Flash => &[
            ("↑ ↓ / k j", "move the menu cursor"),
            ("enter", "run the selected action"),
            ("tab", "move between option fields"),
            ("← →", "cycle an option's value"),
            ("type / backspace", "edit offset or extra flags"),
            ("q / esc", "back one screen, then to the dashboard"),
            ("ctrl+c", "quit"),
        ],
    };

    let mut lines = vec![Line::from("Keyboard".bold()), Line::from("")];
    lines.extend(bindings.iter().map(|(key, description)| {
        Line::from(vec![
            Span::styled(format!("  {key:<18}"), Style::new().fg(Color::Cyan)),
            Span::raw(*description),
        ])
    }));

    let popup = centered(area, 56, lines.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(modal("Help")), popup);
}

fn draw_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize) {
    let options = PickerOption::all();
    let active = app.manager.override_kind();

    let items: Vec<ListItem> = options
        .iter()
        .map(|option| {
            let current = match option {
                PickerOption::Automatic => active.is_none(),
                PickerOption::Backend(kind) => active == Some(*kind),
            };
            let mut spans = vec![Span::raw(format!(" {} ", option.label()))];
            if current {
                spans.push(Span::styled("(active)", Style::new().dim()));
            }
            // Detection's own opinion, so an override is an informed choice.
            if let PickerOption::Backend(kind) = option
                && let Some(detection) = app.manager.detection()
            {
                spans.push(Span::styled(
                    format!("  {:.2}", detection.confidence_of(*kind)),
                    Style::new().fg(Color::Cyan),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let popup = centered(area, 48, options.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal("Backend"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        popup,
        &mut state,
    );
}

fn modal(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
}

/// Centers a `width`x`height` box inside `area`, shrinking to fit --- shared
/// with the flash dialog (`super::centered`), which sizes itself off its own
/// content the same way every modal here does.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    super::centered(area, width, height)
}
