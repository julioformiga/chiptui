//! Modal layers: help and manual backend selection.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, FileAction, Overlay, PickerOption, View, ViewerSource, ViewerState};
use crate::backend::BackendKind;
use crate::browser::SyncPlan;
use crate::highlight::{self, TokenKind};

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(overlay) = app.overlay.clone() else {
        return;
    };
    match overlay {
        Overlay::Help => draw_help(frame, area, app),
        Overlay::BackendPicker { selected } => draw_picker(frame, area, app, selected),
        Overlay::DevicePicker { selected } => draw_device_picker(frame, area, app, selected),
        Overlay::Confirm { message, confirm } => draw_confirm(frame, area, &message, confirm),
        Overlay::ConfirmBuild { action, confirm } => {
            // The message is derived, not stored: whatever the panel would
            // run right now is exactly what the confirm should quote.
            let message = app
                .build
                .as_ref()
                .and_then(|panel| {
                    let backend = app.manager.backend()?;
                    match action {
                        crate::build::BuildAction::Build(kind) => panel
                            .command(kind, backend)
                            .map(|command| command.to_string()),
                        crate::build::BuildAction::Flash => panel
                            .flash_command(backend)
                            .map(|command| command.to_string()),
                        // Only destructive actions reach this overlay.
                        _ => None,
                    }
                })
                .unwrap_or_else(|| "this action".to_string());
            draw_confirm(frame, area, &format!("Run {message}?"), confirm);
        }
        Overlay::BoardPicker { input, selected } => {
            draw_board_picker(frame, area, app, &input, selected)
        }
        Overlay::FirmwarePicker { selected } => draw_firmware_picker(frame, area, app, selected),
        Overlay::ProjectSetup { selected } => draw_project_setup(frame, area, selected),
        Overlay::ConfirmDownloadOverwrite { url, dest, confirm } => {
            draw_confirm_download_overwrite(frame, area, &url, &dest, confirm)
        }
        Overlay::ConfirmUpload {
            name,
            is_dir,
            confirm,
        } => draw_confirm_upload(frame, area, &name, is_dir, confirm),
        Overlay::FileActions {
            side,
            name,
            is_dir,
            status,
            selected,
        } => {
            let is_text = crate::files::is_text_like(&name);
            let actions =
                FileAction::for_entry(side, is_dir, is_text, status, app.manager.capabilities());
            draw_file_actions(frame, area, &name, is_dir, &actions, selected);
        }
        Overlay::FileViewer => draw_file_viewer(frame, area, app),
        Overlay::ConfirmRestartDevice { confirm } => {
            draw_confirm_restart_device(frame, area, app, confirm)
        }
        Overlay::ConfirmEraseForMicroPython { confirm } => {
            draw_confirm_erase_for_micropython(frame, area, confirm)
        }
        Overlay::ConfirmDelete {
            side,
            name,
            is_dir,
            confirm,
        } => draw_confirm_delete(frame, area, side, &name, is_dir, confirm),
        Overlay::CreateEntry { side, input } => draw_create_entry(frame, area, side, &input),
        Overlay::PackageInstall { input } => draw_package_install(frame, area, &input),
        Overlay::SyncPreview { plan, confirm } => draw_sync_preview(frame, area, &plan, confirm),
        Overlay::ConfirmInterruptDevice { confirm } => {
            draw_confirm_interrupt_device(frame, area, confirm)
        }
        Overlay::RestoreDeviceScript { selected } => {
            draw_restore_device_script(frame, area, selected)
        }
    }
}

fn draw_confirm_dialog(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    message: Vec<Line>,
    confirm: bool,
    width: u16,
    height: u16,
) {
    let popup = centered(area, width.min(area.width), height);
    let block = modal(title);
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [message_area, buttons_area] = Layout::vertical([
        Constraint::Length(height.saturating_sub(5)),
        Constraint::Length(3),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .wrap(ratatui::widgets::Wrap { trim: false }),
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

// Reached two ways now: right after a post-edit reupload lands, and from the
// standalone `shift+r` binding --- the wording stays generic so neither
// caller needs a variant of its own just to describe why it fired.
fn draw_confirm_restart_device(frame: &mut Frame, area: Rect, app: &App, confirm: bool) {
    let command = crate::backend::micropython::commands::soft_reset(app.devices.selected_port());
    let message = vec![
        Line::from("Restart the device now (soft-reset)?".fg(Color::Yellow)),
        Line::from(""),
        Line::from(command.to_string().dim()),
    ];
    draw_confirm_dialog(frame, area, "Restart device?", message, confirm, 54, 8);
}

fn draw_confirm_erase_for_micropython(frame: &mut Frame, area: Rect, confirm: bool) {
    let message = vec![
        Line::from("Device is unresponsive to MicroPython commands.".fg(Color::Yellow)),
        Line::from("It might have a different firmware (e.g. Zephyr) installed.".fg(Color::Yellow)),
        Line::from(""),
        Line::from("Would you like to install MicroPython?".fg(Color::White)),
    ];
    draw_confirm_dialog(frame, area, "Install MicroPython?", message, confirm, 65, 9);
}

/// A device command is waiting on the user because the board is believed to
/// be running a script: `mpremote` interrupts it (Ctrl-C, then raw REPL) for
/// every filesystem operation, so the interruption is the user's call, not a
/// silent side effect. Naming what happens afterwards matters as much as the
/// warning --- the script can be brought back.
fn draw_confirm_interrupt_device(frame: &mut Frame, area: Rect, confirm: bool) {
    let message = vec![
        Line::from("A script is running on the device.".fg(Color::Yellow)),
        Line::from("Every mpremote command interrupts it (Ctrl-C) to use the REPL.".dim()),
        Line::from(""),
        Line::from("Run the pending device command anyway?".fg(Color::White)),
        Line::from("Afterwards you can restart the script from the prompt that follows.".dim()),
    ];
    draw_confirm_dialog(frame, area, "Device is busy", message, confirm, 64, 10);
}

/// How to bring back a script that was interrupted for a device operation:
/// a three-row picker, since "restart" honestly splits into a clean reset
/// and a fast relaunch with different tradeoffs. "Leave it stopped" is the
/// default highlight --- restarting re-runs code the user may still be
/// changing.
fn draw_restore_device_script(frame: &mut Frame, area: Rect, selected: usize) {
    const CHOICES: [(&str, &str); 3] = [
        ("Reset the board", "reboot; runs boot.py + main.py again"),
        (
            "Restart main.py",
            "no reboot; leftover state from the interrupted run",
        ),
        (
            "Leave it stopped",
            "start it yourself later (m, or reset by hand)",
        ),
    ];

    let items: Vec<ListItem> = CHOICES
        .iter()
        .map(|(label, detail)| {
            ListItem::new(Line::from(vec![
                Span::raw(format!(" {label} ")),
                Span::styled(format!("— {detail}"), Style::new().dim()),
            ]))
        })
        .collect();

    let popup = centered(area, 64, CHOICES.len() as u16 + 4);
    let block = modal("Restart device script?");
    let inner = block.inner(popup);
    let [message, list] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(CHOICES.len() as u16)])
            .areas(inner);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new("The script that was interrupted can be brought back:".dim()),
        message,
    );

    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        list,
        &mut state,
    );
}

fn draw_confirm_delete(
    frame: &mut Frame,
    area: Rect,
    side: crate::browser::Side,
    name: &str,
    is_dir: bool,
    confirm: bool,
) {
    let side_str = match side {
        crate::browser::Side::Local => "locally",
        crate::browser::Side::Device => "from device",
    };
    let label = if is_dir {
        format!("Delete '{}/' and everything in it?", name)
    } else {
        format!("Delete '{}'?", name)
    };
    let message = vec![
        Line::from(label.fg(Color::Yellow)),
        Line::from(format!("This will remove it {}.", side_str).dim()),
    ];
    draw_confirm_dialog(frame, area, "Confirm Delete", message, confirm, 54, 9);
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

/// Renders the action menu for the entry `enter` was pressed on. The caller
/// resolves the entry's verdict ([`FileAction::for_entry`]) so this stays a
/// pure drawing function of an already-decided action list, sized like the
/// other small pickers in this module.
fn draw_file_actions(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    is_dir: bool,
    actions: &[FileAction],
    selected: usize,
) {
    let items: Vec<ListItem> = actions
        .iter()
        .map(|action| ListItem::new(Line::from(format!(" {} ", action.label()))))
        .collect();

    let popup = centered(area, 44, actions.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));
    let title = if is_dir {
        format!("{name}/")
    } else {
        name.to_string()
    };

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal(&title))
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
            let is_diff = matches!(viewer.source, ViewerSource::Diff { .. });
            let rendered: Vec<Line> = if is_diff {
                lines
                    .iter()
                    .map(|line| Line::from(diff_line_spans(line)))
                    .collect()
            } else {
                let language = highlight::Language::from_filename(&name);
                lines
                    .iter()
                    .map(|line| {
                        Line::from(
                            highlight::highlight_line(line, language)
                                .into_iter()
                                .map(|token| Span::styled(token.text, token_style(token.kind)))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect()
            };
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

/// Colours one unified-diff line by its leading marker: added lines green,
/// removed lines red, hunk headers (`@@`) cyan, and unchanged context plain.
/// The whole line shares one colour rather than per-token highlighting so the
/// diff's added/removed regions read at a glance, the way `diff --color` does.
fn diff_line_spans(line: &str) -> Vec<Span<'_>> {
    let style = match line.chars().next() {
        Some('+') => Style::new().fg(Color::Green),
        Some('-') => Style::new().fg(Color::Red),
        Some('@') => Style::new().fg(Color::Cyan),
        _ => Style::new(),
    };
    vec![Span::styled(line.to_string(), style)]
}

/// A firmware download would overwrite a file already in the project root;
/// mirrors [`draw_confirm`]'s "show exactly what is about to happen" rule,
/// applied to a filesystem write rather than a device operation.
fn draw_confirm_download_overwrite(
    frame: &mut Frame,
    area: Rect,
    url: &str,
    dest: &std::path::Path,
    confirm: bool,
) {
    let message = vec![
        Line::from(format!("{} already exists.", dest.display()).fg(Color::Yellow)),
        Line::from(format!("Overwrite it by downloading {url}?")),
    ];
    draw_confirm_dialog(frame, area, "Overwrite firmware?", message, confirm, 70, 8);
}

fn draw_confirm_upload(frame: &mut Frame, area: Rect, name: &str, is_dir: bool, confirm: bool) {
    let message = if is_dir {
        vec![
            Line::from(
                format!("Upload '{}/' and everything in it to the device?", name).fg(Color::Yellow),
            ),
            Line::from(
                "This will overwrite any existing files with the same names on the device.".dim(),
            ),
        ]
    } else {
        vec![
            Line::from(format!("Upload '{}' to the device?", name).fg(Color::Yellow)),
            Line::from(
                "This will overwrite any existing file with the same name on the device.".dim(),
            ),
        ]
    };
    draw_confirm_dialog(frame, area, "Confirm Upload", message, confirm, 65, 8);
}

/// Inline text entry for creating a file or directory (`a`), in whichever
/// pane last had focus --- a trailing `/` on the typed name is what decides
/// file vs directory, explained right in the box so the rule needs no
/// separate help lookup.
fn draw_create_entry(frame: &mut Frame, area: Rect, side: crate::browser::Side, input: &str) {
    let popup = centered(area, 54, 6);
    let title = match side {
        crate::browser::Side::Local => "New (local)",
        crate::browser::Side::Device => "New (device)",
    };
    let block = modal(title);
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [hint_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(3)]).areas(inner);

    frame.render_widget(
        Paragraph::new("name, or 'name/' for a directory".dim()),
        hint_area,
    );

    let field = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan));
    let field_inner = field.inner(input_area);
    frame.render_widget(field, input_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(input),
            Span::raw("_").fg(Color::Cyan),
        ])),
        field_inner,
    );
}

/// Inline text entry for `mip install` (`i` on the device pane) --- same
/// shape as [`draw_create_entry`], just with no `side` (it always acts on
/// the device) and a hint describing a package spec instead of a filename.
fn draw_package_install(frame: &mut Frame, area: Rect, input: &str) {
    let popup = centered(area, 54, 6);
    let block = modal("Install package (mip)");
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [hint_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(3)]).areas(inner);

    frame.render_widget(
        Paragraph::new("package name, e.g. urequests, or name@version, github:org/repo".dim()),
        hint_area,
    );

    let field = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan));
    let field_inner = field.inner(input_area);
    frame.render_widget(field, input_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(input),
            Span::raw("_").fg(Color::Cyan),
        ])),
        field_inner,
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
fn draw_confirm(frame: &mut Frame, area: Rect, message: &str, confirm: bool) {
    let lines = vec![Line::from(message.to_string().fg(Color::Yellow))];
    draw_confirm_dialog(frame, area, "Confirm", lines, confirm, 70, 7);
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

/// Board target selection: a filter box over the `west boards` list
/// (`SPEC.md` §10 --- hundreds of targets make raw navigation useless, and
/// the choice is session-only, so the header of the modal says so instead of
/// letting the user discover it from a changed file).
fn draw_board_picker(frame: &mut Frame, area: Rect, app: &App, input: &str, selected: usize) {
    use crate::build::BoardsState;

    const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

    // Fixed height: the list scrolls inside it (ListState keeps the
    // selection visible), so the modal does not jump around as the filter
    // changes.
    let height = 20u16;
    let width = 72u16;
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);

    let Some(panel) = app.build.as_ref() else {
        return;
    };

    let title = match &panel.board {
        Some(choice) => match choice.origin {
            crate::build::BoardOrigin::Cache => {
                format!(
                    "Board (build/ says {} — pick to change for this session)",
                    choice.name
                )
            }
            crate::build::BoardOrigin::Picked => {
                format!("Board ({} picked this session)", choice.name)
            }
        },
        None => "Board (none set — the first build needs one)".to_string(),
    };

    let [filter_area, hint_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .areas(modal(&title).inner(popup));

    frame.render_widget(modal(&title), popup);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("filter ", Style::new().dim()),
            Span::raw(input.to_string()),
            Span::styled("▏", Style::new().fg(Color::Cyan)),
        ])),
        filter_area,
    );

    let boards = panel.filtered_boards(input);
    let hint = match &panel.boards {
        BoardsState::Idle | BoardsState::Loading => {
            let spinner = SPINNER[(app.ticks as usize) % SPINNER.len()];
            format!("{spinner} running west boards…")
        }
        BoardsState::Failed(error) => error.clone(),
        BoardsState::Loaded(_) if boards.is_empty() => "no board matches".to_string(),
        BoardsState::Loaded(_) => {
            format!(
                "{} of {} targets",
                boards.len(),
                match &panel.boards {
                    BoardsState::Loaded(all) => all.len(),
                    _ => 0,
                }
            )
        }
    };
    frame.render_widget(
        Paragraph::new(hint.dim()).wrap(ratatui::widgets::Wrap { trim: false }),
        hint_area,
    );

    let items: Vec<ListItem> = boards
        .iter()
        .map(|board| {
            ListItem::new(Line::from(vec![
                Span::raw(format!(" {} ", board.name)),
                board.description.clone().dim(),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        list_area,
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
                "enter",
                "open the menu for the selected entry (send/download, run, view, edit, delete)",
            ),
            ("→", "descend into the selected directory directly"),
            (
                "backspace / ←",
                "go to the parent directory in the file browser",
            ),
            (
                "a",
                "create a file, or a directory if the name ends with '/'",
            ),
            ("c", "compare the selected file by sha256"),
            (
                "shift+s",
                "sync local files to the device (upload missing/changed)",
            ),
            ("h", "show or hide dot-files in the file browser"),
            (
                "enter (build pane)",
                "run the selected build action; stop heads the list while one runs",
            ),
            ("d", "scan for devices (when the backend has a filesystem)"),
            (
                "i",
                "on the device pane: install a package via mip (when the backend supports it)",
            ),
            (
                "m",
                "open the device monitor/REPL (when the backend supports it); ctrl+] exits it",
            ),
            (
                "shift+r",
                "restart the device with a soft-reset (when the backend supports it)",
            ),
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

fn draw_sync_preview(frame: &mut Frame, area: Rect, plan: &SyncPlan, confirm: bool) {
    let mut lines = Vec::new();

    if plan.is_empty() {
        lines.push(Line::from(
            "Nothing to do \u{2014} local and device are in sync.".dim(),
        ));
        let height = 7u16;
        draw_confirm_dialog(frame, area, "Sync preview", lines, confirm, 58, height);
        return;
    }

    if !plan.uploads.is_empty() {
        lines.push(Line::from(
            format!("Upload {} file(s):", plan.uploads.len()).bold(),
        ));
        for (_, device) in plan.uploads.iter().take(8) {
            lines.push(Line::from(format!("  \u{2192} {device}")));
        }
        if plan.uploads.len() > 8 {
            lines.push(Line::from(
                format!("  ... and {} more", plan.uploads.len() - 8).dim(),
            ));
        }
        lines.push(Line::from(""));
    }

    if !plan.mkdirs.is_empty() {
        lines.push(Line::from(
            format!(
                "Create {} director{}:",
                plan.mkdirs.len(),
                if plan.mkdirs.len() == 1 { "y" } else { "ies" }
            )
            .bold(),
        ));
        for dir in plan.mkdirs.iter().take(8) {
            lines.push(Line::from(format!("  + {dir}/")));
        }
        if plan.mkdirs.len() > 8 {
            lines.push(Line::from(
                format!("  ... and {} more", plan.mkdirs.len() - 8).dim(),
            ));
        }
        lines.push(Line::from(""));
    }

    if !plan.deletes.is_empty() {
        lines.push(Line::from(
            format!("Delete {} device-only file(s):", plan.deletes.len()).fg(Color::Yellow),
        ));
        for path in plan.deletes.iter().take(8) {
            lines.push(Line::from(format!("  \u{2717} {path}").fg(Color::Yellow)));
        }
        if plan.deletes.len() > 8 {
            lines.push(Line::from(
                format!("  ... and {} more", plan.deletes.len() - 8).dim(),
            ));
        }
    }

    let height = (lines.len() as u16 + 5).min(area.height.saturating_sub(2));
    draw_confirm_dialog(frame, area, "Sync preview", lines, confirm, 60, height);
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
