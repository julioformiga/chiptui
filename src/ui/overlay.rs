//! Modal layers: help and manual backend selection.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};

use crate::app::help::{self, HelpSection};
use crate::app::{App, FileAction, Overlay, PickerOption, ViewerSource, ViewerState};
use crate::backend::BackendKind;
use crate::browser::SyncPlan;
use crate::highlight::{self, TokenKind};
use crate::ui::{Palette, muted_style, selection_style};

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
    let Some(overlay) = app.overlay.clone() else {
        return;
    };
    match overlay {
        Overlay::Help {
            filter,
            filtering,
            selected,
        } => draw_help(frame, area, app, &filter, filtering, selected, palette),
        Overlay::BackendPicker { selected } => draw_picker(frame, area, app, selected, palette),
        Overlay::DevicePicker { selected } => {
            draw_device_picker(frame, area, app, selected, palette)
        }
        Overlay::ThemePicker { selected } => draw_theme_picker(frame, area, app, selected, palette),
        Overlay::Confirm { message, confirm } => {
            draw_confirm(frame, area, &message, confirm, palette)
        }
        Overlay::ConfirmBuild {
            action: crate::build::BuildAction::UpdateZephyr,
            confirm,
        } => {
            // Derived like every other arm here: whatever the workspace pane
            // would run right now is what the confirm quotes. The literal
            // command (env vars and the venv's west path included) is
            // longer than one dialog line, so it is shortened from the left
            // --- its tail, not its /tmp prefix, is its identity. A distinct
            // dialog (not `draw_confirm`'s single line) because `west
            // update` rewrites a shared installation, not just this
            // project --- the extra line says so.
            let command = app
                .workspace
                .as_ref()
                .and_then(|panel| {
                    let backend = app.manager.backend()?;
                    panel
                        .update_command(backend)
                        .map(|command| command.to_string())
                })
                .unwrap_or_else(|| "this action".to_string());
            let quoted = shorten_tail(&command, 63);
            let lines = vec![
                Line::from("Run this in the shared workspace?".fg(palette.warning)),
                Line::from(quoted).fg(palette.fg),
                Line::from(
                    "It rewrites the checkouts every project in it shares.".fg(palette.muted),
                ),
            ];
            draw_confirm_dialog(frame, area, "Confirm", lines, confirm, (72, 8), palette);
        }
        Overlay::ConfirmBuild { action, confirm } => {
            // The message is derived, not stored: whatever the panel would
            // run right now is exactly what the confirm should quote. The
            // literal command (the venv's west path and `ZEPHYR_BASE` env
            // included) regularly exceeds one dialog line, so it is
            // shortened from the left --- its tail, not its /tmp prefix,
            // is its identity.
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
            draw_confirm(
                frame,
                area,
                &format!("Run {}?", shorten_tail(&message, 60)),
                confirm,
                palette,
            );
        }
        Overlay::BoardPicker { input, selected } => {
            draw_board_picker(frame, area, app, &input, selected, palette)
        }
        Overlay::ShieldPicker { input, selected } => {
            draw_shield_picker(frame, area, app, &input, selected, palette)
        }
        Overlay::DirPicker {
            purpose,
            path,
            selected,
            error,
        } => draw_dir_picker(
            frame,
            area,
            purpose,
            &path,
            selected,
            error.as_deref(),
            palette,
        ),
        Overlay::BuildDirPicker { input, selected } => {
            draw_build_dir_picker(frame, area, app, &input, selected, palette)
        }
        Overlay::ProjectPicker { selected, error } => {
            draw_project_picker(frame, area, app, selected, error.as_deref(), palette)
        }
        Overlay::FirmwarePicker { selected } => {
            draw_firmware_picker(frame, area, app, selected, palette)
        }
        Overlay::ProjectSetup { selected } => draw_project_setup(frame, area, selected, palette),
        Overlay::ConfirmDownloadOverwrite { url, dest, confirm } => {
            draw_confirm_download_overwrite(frame, area, &url, &dest, confirm, palette)
        }
        Overlay::ConfirmUpload {
            name,
            is_dir,
            confirm,
        } => draw_confirm_upload(frame, area, &name, is_dir, confirm, palette),
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
            draw_file_actions(frame, area, &name, is_dir, &actions, selected, palette);
        }
        Overlay::FileViewer => draw_file_viewer(frame, area, app, palette),
        Overlay::ConfirmRestartDevice { confirm } => {
            draw_confirm_restart_device(frame, area, app, confirm, palette)
        }
        Overlay::ConfirmEraseForMicroPython { confirm } => {
            draw_confirm_erase_for_micropython(frame, area, confirm, palette)
        }
        Overlay::ConfirmSwitchProject { confirm } => {
            draw_confirm_switch_project(frame, area, app, confirm, palette)
        }
        Overlay::ConfirmDelete {
            side,
            name,
            is_dir,
            confirm,
        } => draw_confirm_delete(frame, area, side, &name, is_dir, confirm, palette),
        Overlay::CreateEntry { side, input } => {
            draw_create_entry(frame, area, side, &input, palette)
        }
        Overlay::RenameEntry { name, input } => {
            draw_rename_entry(frame, area, &name, &input, palette)
        }
        Overlay::PackageInstall { input } => draw_package_install(frame, area, &input, palette),
        Overlay::SyncPreview { plan, confirm } => {
            draw_sync_preview(frame, area, &plan, confirm, palette)
        }
        Overlay::ConfirmInterruptDevice { confirm } => {
            draw_confirm_interrupt_device(frame, area, confirm, palette)
        }
        Overlay::RestoreDeviceScript { selected } => {
            draw_restore_device_script(frame, area, selected, palette)
        }
    }
}

fn draw_confirm_dialog(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    message: Vec<Line>,
    confirm: bool,
    size: (u16, u16),
    palette: Palette,
) {
    let (width, height) = size;
    let popup = centered(area, width.min(area.width), height);
    let block = modal(title, palette);
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

    draw_dialog_button(frame, no_area, "No", !confirm, palette);
    draw_dialog_button(frame, yes_area, "Yes", confirm, palette);
}

// Reached two ways now: right after a post-edit reupload lands, and from the
// standalone `shift+r` binding --- the wording stays generic so neither
// caller needs a variant of its own just to describe why it fired.
fn draw_confirm_restart_device(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    confirm: bool,
    palette: Palette,
) {
    let command = crate::backend::micropython::commands::soft_reset(app.devices.selected_port());
    let message = vec![
        Line::from("Restart the device now (soft-reset)?".fg(palette.warning)),
        Line::from(""),
        Line::from(command.to_string().fg(palette.muted)),
    ];
    draw_confirm_dialog(
        frame,
        area,
        "Restart device?",
        message,
        confirm,
        (54, 8),
        palette,
    );
}

/// Leaving the project while work is in flight. The count is the whole
/// point: "2 commands" is what tells the user whether the build they started
/// is the thing about to die.
fn draw_confirm_switch_project(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    confirm: bool,
    palette: Palette,
) {
    let running = app.running_commands();
    let message = vec![
        Line::from(
            format!(
                "{running} command{} still running in this project.",
                if running == 1 { " is" } else { "s are" }
            )
            .fg(palette.warning),
        ),
        Line::from("Going back to the project list cancels them.".fg(palette.muted)),
        Line::from(""),
        Line::from("Leave this project?".fg(palette.fg)),
    ];
    draw_confirm_dialog(
        frame,
        area,
        "Switch project?",
        message,
        confirm,
        (62, 9),
        palette,
    );
}

fn draw_confirm_erase_for_micropython(
    frame: &mut Frame,
    area: Rect,
    confirm: bool,
    palette: Palette,
) {
    let message = vec![
        Line::from("Device is unresponsive to MicroPython commands.".fg(palette.warning)),
        Line::from(
            "It might have a different firmware (e.g. Zephyr) installed.".fg(palette.warning),
        ),
        Line::from(""),
        Line::from("Would you like to install MicroPython?".fg(palette.fg)),
    ];
    draw_confirm_dialog(
        frame,
        area,
        "Install MicroPython?",
        message,
        confirm,
        (65, 9),
        palette,
    );
}

/// A device command is waiting on the user because the board is believed to
/// be running a script: `mpremote` interrupts it (Ctrl-C, then raw REPL) for
/// every filesystem operation, so the interruption is the user's call, not a
/// silent side effect. Naming what happens afterwards matters as much as the
/// warning --- the script can be brought back.
fn draw_confirm_interrupt_device(frame: &mut Frame, area: Rect, confirm: bool, palette: Palette) {
    let message = vec![
        Line::from("A script is running on the device.".fg(palette.warning)),
        Line::from(
            "Every mpremote command interrupts it (Ctrl-C) to use the REPL.".fg(palette.muted),
        ),
        Line::from(""),
        Line::from("Run the pending device command anyway?".fg(palette.fg)),
        Line::from(
            "Afterwards you can restart the script from the prompt that follows.".fg(palette.muted),
        ),
    ];
    draw_confirm_dialog(
        frame,
        area,
        "Device is busy",
        message,
        confirm,
        (64, 10),
        palette,
    );
}

/// How to bring back a script that was interrupted for a device operation:
/// a three-row picker, since "restart" honestly splits into a clean reset
/// and a fast relaunch with different tradeoffs. "Leave it stopped" is the
/// default highlight --- restarting re-runs code the user may still be
/// changing.
fn draw_restore_device_script(frame: &mut Frame, area: Rect, selected: usize, palette: Palette) {
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
                Span::styled(format!(" {label} "), Style::new().fg(palette.fg)),
                Span::styled(format!("— {detail}"), muted_style(palette)),
            ]))
        })
        .collect();

    let popup = centered(area, 64, CHOICES.len() as u16 + 4);
    let block = modal("Restart device script?", palette);
    let inner = block.inner(popup);
    let [message, list] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(CHOICES.len() as u16)])
            .areas(inner);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new("The script that was interrupted can be brought back:".fg(palette.muted)),
        message,
    );

    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
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
    palette: Palette,
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
        Line::from(label.fg(palette.warning)),
        Line::from(format!("This will remove it {}.", side_str).fg(palette.muted)),
    ];
    draw_confirm_dialog(
        frame,
        area,
        "Confirm Delete",
        message,
        confirm,
        (54, 9),
        palette,
    );
}

/// One btop-style dialog button: a bordered box, filled solid when selected
/// so the highlighted choice reads at a glance rather than needing the
/// border colour alone to carry it (the same "never rely on colour alone"
/// reasoning as the file panes' sync markers, `ui/files.rs`).
fn draw_dialog_button(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    selected: bool,
    palette: Palette,
) {
    let (border_style, text_style) = if selected {
        (
            Style::new().fg(palette.accent),
            Style::new()
                .fg(palette.bg)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (muted_style(palette), muted_style(palette))
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
    palette: Palette,
) {
    let items: Vec<ListItem> = actions
        .iter()
        .map(|action| {
            ListItem::new(Line::from(Span::styled(
                format!(" {} ", action.label()),
                Style::new().fg(palette.fg),
            )))
        })
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
            .block(modal(&title, palette))
            .highlight_style(selection_style(palette)),
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
fn draw_file_viewer(frame: &mut Frame, area: Rect, app: &mut App, palette: Palette) {
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
    let block = modal(&title, palette);
    let inner = block.inner(popup);
    app.viewer_viewport = inner.height.max(1) as usize;

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    match &viewer.state {
        ViewerState::Loading => {
            frame.render_widget(Paragraph::new("loading…".fg(palette.muted)), inner);
        }
        ViewerState::Error(message) => {
            frame.render_widget(
                Paragraph::new(message.clone().fg(palette.warning))
                    .wrap(ratatui::widgets::Wrap { trim: false }),
                inner,
            );
        }
        ViewerState::Ready { lines } => {
            let is_diff = matches!(viewer.source, ViewerSource::Diff { .. });
            let rendered: Vec<Line> = if is_diff {
                lines
                    .iter()
                    .map(|line| Line::from(diff_line_spans(line, palette)))
                    .collect()
            } else {
                let language = highlight::Language::from_filename(&name);
                lines
                    .iter()
                    .map(|line| {
                        Line::from(
                            highlight::highlight_line(line, language)
                                .into_iter()
                                .map(|token| {
                                    Span::styled(token.text, token_style(token.kind, palette))
                                })
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

/// A command too long for one dialog line, cut from the left: the tail
/// (program name, arguments) is what identifies it, not the `/tmp` or
/// workspace prefix the environment puts in front.
fn shorten_tail(text: &str, max_chars: usize) -> String {
    let length = text.chars().count();
    if length <= max_chars {
        text.to_string()
    } else {
        format!(
            "…{}",
            text.chars()
                .skip(length - (max_chars - 1))
                .collect::<String>()
        )
    }
}

fn token_style(kind: TokenKind, palette: Palette) -> Style {
    match kind {
        TokenKind::Plain => Style::new().fg(palette.fg),
        TokenKind::Keyword => Style::new().fg(palette.secondary),
        TokenKind::String => Style::new().fg(palette.success),
        TokenKind::Comment => Style::new().fg(palette.muted).italic(),
        TokenKind::Number => Style::new().fg(palette.accent),
    }
}

/// Colours one unified-diff line by its leading marker: added lines green,
/// removed lines red, hunk headers (`@@`) cyan, and unchanged context plain.
/// The whole line shares one colour rather than per-token highlighting so the
/// diff's added/removed regions read at a glance, the way `diff --color` does.
fn diff_line_spans(line: &str, palette: Palette) -> Vec<Span<'_>> {
    let style = match line.chars().next() {
        Some('+') => Style::new().fg(palette.success),
        Some('-') => Style::new().fg(palette.error),
        Some('@') => Style::new().fg(palette.accent),
        _ => Style::new().fg(palette.fg),
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
    palette: Palette,
) {
    let message = vec![
        Line::from(format!("{} already exists.", dest.display()).fg(palette.warning)),
        Line::from(format!("Overwrite it by downloading {url}?").fg(palette.fg)),
    ];
    draw_confirm_dialog(
        frame,
        area,
        "Overwrite firmware?",
        message,
        confirm,
        (70, 8),
        palette,
    );
}

fn draw_confirm_upload(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    is_dir: bool,
    confirm: bool,
    palette: Palette,
) {
    let message = if is_dir {
        vec![
            Line::from(
                format!("Upload '{}/' and everything in it to the device?", name)
                    .fg(palette.warning),
            ),
            Line::from(
                "This will overwrite any existing files with the same names on the device."
                    .fg(palette.muted),
            ),
        ]
    } else {
        vec![
            Line::from(format!("Upload '{}' to the device?", name).fg(palette.warning)),
            Line::from(
                "This will overwrite any existing file with the same name on the device."
                    .fg(palette.muted),
            ),
        ]
    };
    draw_confirm_dialog(
        frame,
        area,
        "Confirm Upload",
        message,
        confirm,
        (65, 8),
        palette,
    );
}

/// Inline text entry for creating a file or directory (`a`), in whichever
/// pane last had focus --- a trailing `/` on the typed name is what decides
/// file vs directory, explained right in the box so the rule needs no
/// separate help lookup.
fn draw_create_entry(
    frame: &mut Frame,
    area: Rect,
    side: crate::browser::Side,
    input: &str,
    palette: Palette,
) {
    let popup = centered(area, 54, 6);
    let title = match side {
        crate::browser::Side::Local => "New (local)",
        crate::browser::Side::Device => "New (device)",
    };
    let block = modal(title, palette);
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [hint_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(3)]).areas(inner);

    frame.render_widget(
        Paragraph::new("name, or 'name/' for a directory".fg(palette.muted)),
        hint_area,
    );

    let field = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette.accent));
    let field_inner = field.inner(input_area);
    frame.render_widget(field, input_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(input.to_string(), Style::new().fg(palette.fg)),
            Span::raw("_").fg(palette.accent),
        ])),
        field_inner,
    );
}

/// Inline text entry for renaming an entry (`r` in the workspace file list)
/// --- same shape as [`draw_create_entry`], with the current name shown above
/// a field pre-filled with it, so editing starts from the end of what is
/// already there and an unedited confirm visibly changes nothing.
fn draw_rename_entry(frame: &mut Frame, area: Rect, name: &str, input: &str, palette: Palette) {
    let popup = centered(area, 54, 6);
    let block = modal("Rename", palette);
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [hint_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(3)]).areas(inner);

    frame.render_widget(
        Paragraph::new(format!("current name: {name}").fg(palette.muted)),
        hint_area,
    );

    let field = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette.accent));
    let field_inner = field.inner(input_area);
    frame.render_widget(field, input_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(input.to_string(), Style::new().fg(palette.fg)),
            Span::raw("_").fg(palette.accent),
        ])),
        field_inner,
    );
}

/// Inline text entry for `mip install` (`i` on the device pane) --- same
/// shape as [`draw_create_entry`], just with no `side` (it always acts on
/// the device) and a hint describing a package spec instead of a filename.
fn draw_package_install(frame: &mut Frame, area: Rect, input: &str, palette: Palette) {
    let popup = centered(area, 54, 6);
    let block = modal("Install package (mip)", palette);
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [hint_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(3)]).areas(inner);

    frame.render_widget(
        Paragraph::new(
            "package name, e.g. urequests, or name@version, github:org/repo".fg(palette.muted),
        ),
        hint_area,
    );

    let field = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette.accent));
    let field_inner = field.inner(input_area);
    frame.render_widget(field, input_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(input.to_string(), Style::new().fg(palette.fg)),
            Span::raw("_").fg(palette.accent),
        ])),
        field_inner,
    );
}

/// Empty or unrecognized project (`SPEC.md` §7): asks which backend this
/// directory is, offering no "Automatic" row since detection already failed
/// to conclude one.
fn draw_project_setup(frame: &mut Frame, area: Rect, selected: usize, palette: Palette) {
    let items: Vec<ListItem> = BackendKind::ALL
        .iter()
        .map(|kind| {
            ListItem::new(Line::from(Span::styled(
                format!(" {} ", kind.display_name()),
                Style::new().fg(palette.fg),
            )))
        })
        .collect();

    let popup = centered(area, 60, BackendKind::ALL.len() as u16 + 4);
    let block = modal("New project", palette);
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
                .fg(palette.muted),
        )
        .wrap(ratatui::widgets::Wrap { trim: false }),
        message,
    );

    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
        list,
        &mut state,
    );
}

/// A destructive esptool action awaiting explicit confirmation (`SPEC.md`
/// §15). `message` is always the literal command about to run, never a
/// paraphrase, so shown as-is.
fn draw_confirm(frame: &mut Frame, area: Rect, message: &str, confirm: bool, palette: Palette) {
    let lines = vec![Line::from(message.to_string().fg(palette.warning))];
    draw_confirm_dialog(frame, area, "Confirm", lines, confirm, (70, 7), palette);
}

/// Installation-directory selection: a real filesystem browser --- "use
/// this directory" first, the parent, then the subdirectories. The
/// validation error (with the install guide) sits under the list when an
/// accepted directory was not a Zephyr installation.
fn draw_dir_picker(
    frame: &mut Frame,
    area: Rect,
    purpose: crate::workspace::DirPurpose,
    path: &std::path::Path,
    selected: usize,
    error: Option<&str>,
    palette: Palette,
) {
    let title = match purpose {
        crate::workspace::DirPurpose::Installation => "Where is the Zephyr installation?",
        crate::workspace::DirPurpose::Projects => "Where are your Zephyr projects?",
    };
    let height = 18u16;
    let width = 72u16;
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    let block = modal(title, palette);
    frame.render_widget(block.clone(), popup);

    let inner = block.inner(popup);
    let [path_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("dir  ", muted_style(palette)),
            Span::styled(path.display().to_string(), Style::new().fg(palette.fg)),
        ])),
        path_area,
    );

    let (rows, read_error) = crate::workspace::dir_rows(path);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row.kind {
            crate::workspace::DirRowKind::Use => ListItem::new(Line::from(vec![
                Span::styled("→ ", Style::new().fg(palette.accent)),
                "use this directory".fg(palette.fg).bold(),
            ])),
            crate::workspace::DirRowKind::Parent | crate::workspace::DirRowKind::Dir => {
                ListItem::new(Line::from(Span::styled(
                    format!("  {}", row.name),
                    Style::new().fg(palette.fg),
                )))
            }
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
        list_area,
        &mut state,
    );

    let footer = match (error, read_error.as_deref()) {
        (Some(error), _) => Line::from(error.to_string().fg(palette.error)),
        (None, Some(read)) => Line::from(read.fg(palette.warning)),
        (None, None) => Line::from(
            "enter: open / accept · ←: up · esc: cancel — the choice is saved to the config"
                .fg(palette.muted),
        ),
    };
    frame.render_widget(
        Paragraph::new(footer).wrap(ratatui::widgets::Wrap { trim: false }),
        footer_area,
    );
}

/// Project selection from the configured projects folder: every immediate
/// subdirectory, the buildable ones carrying the elements `west build`
/// needs and the rest saying so out loud --- the verification the gate
/// promises, visible before Enter is ever pressed (`SPEC.md` §14).
fn draw_project_picker(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    selected: usize,
    error: Option<&str>,
    palette: Palette,
) {
    let height = 18u16;
    let width = 72u16;
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    let block = modal("Which project?", palette);
    frame.render_widget(block.clone(), popup);

    let inner = block.inner(popup);
    let [dir_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(inner);

    let dir = app
        .workspace
        .as_ref()
        .and_then(|panel| panel.projects.clone());
    let Some(dir) = dir else {
        frame.render_widget(
            Paragraph::new("no projects folder configured".fg(palette.warning)),
            dir_area,
        );
        return;
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("in   ", muted_style(palette)),
            Span::styled(dir.display().to_string(), Style::new().fg(palette.fg)),
        ])),
        dir_area,
    );

    let (rows, read_error) = crate::backend::zephyr::projects::project_rows(&dir);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            if row.buildable {
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    row.name.clone().fg(palette.fg).bold(),
                    Span::raw("  "),
                    Span::styled("✓ CMakeLists.txt", Style::new().fg(palette.success)),
                ]))
            } else {
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(row.name.clone(), Style::new().fg(palette.fg)),
                    Span::raw("  "),
                    Span::styled("no CMakeLists.txt", muted_style(palette)),
                ]))
            }
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
        list_area,
        &mut state,
    );

    let footer = if let Some(error) = error {
        Line::from(error.to_string().fg(palette.error))
    } else if let Some(read) = read_error {
        Line::from(read.fg(palette.warning))
    } else if rows.is_empty() {
        Line::from(
            "no subdirectories here — put an application in the folder, or choose another"
                .fg(palette.warning),
        )
    } else {
        Line::from(
            "enter: build this one · esc: cancel — the choice is session-only".fg(palette.muted),
        )
    };
    frame.render_widget(
        Paragraph::new(footer).wrap(ratatui::widgets::Wrap { trim: false }),
        footer_area,
    );
}

/// Build-directory selection: the project's configured directories plus a
/// typed new name (`west build -d`).
fn draw_build_dir_picker(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    input: &str,
    selected: usize,
    palette: Palette,
) {
    let height = 16u16;
    let width = 60u16;
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);

    let Some(panel) = app.build.as_ref() else {
        return;
    };
    let title = format!("Build directory (currently {})", panel.build_dir);
    frame.render_widget(modal(&title, palette), popup);

    let [filter_area, hint_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .areas(modal(&title, palette).inner(popup));

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("name  ", muted_style(palette)),
            Span::styled(input.to_string(), Style::new().fg(palette.fg)),
            Span::styled("▏", Style::new().fg(palette.accent)),
        ])),
        filter_area,
    );

    let dirs = panel.filtered_build_dirs(input);
    let hint = if input.trim().is_empty() {
        "Enter picks; type a new name to create another".to_string()
    } else if dirs.is_empty() {
        "not a name: no path separators".to_string()
    } else {
        format!("{} of {} directories", dirs.len(), dirs.len())
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(hint.fg(palette.muted)), Line::from("")]),
        hint_area,
    );

    let items: Vec<ListItem> = dirs
        .iter()
        .map(|dir| {
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {dir} "), Style::new().fg(palette.fg)),
                Span::styled(
                    if *dir == crate::build::DEFAULT_BUILD_DIR {
                        "default"
                    } else {
                        ""
                    },
                    muted_style(palette),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
        list_area,
        &mut state,
    );
}

/// Chooses among several `.bin`/`.elf` candidates found in the project root.
fn draw_firmware_picker(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    selected: usize,
    palette: Palette,
) {
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
                "No .bin/.elf firmware found in the project root.".fg(palette.warning),
            )])
            .block(modal("Firmware", palette)),
            popup,
        );
        return;
    }

    let items: Vec<ListItem> = firmware
        .iter()
        .map(|entry| {
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", entry.name), Style::new().fg(palette.fg)),
                Span::styled(format!("{} bytes", entry.size), muted_style(palette)),
            ]))
        })
        .collect();

    let popup = centered(area, 64, firmware.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal("Firmware", palette))
            .highlight_style(selection_style(palette)),
        popup,
        &mut state,
    );
}

/// Board target selection: a filter box over the `west boards` list
/// (`SPEC.md` §10 --- hundreds of targets make raw navigation useless, and
/// the choice is saved with the project, so the header of the modal says
/// where the current answer comes from instead of letting the user
/// discover it from a changed file).
fn draw_board_picker(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    input: &str,
    selected: usize,
    palette: Palette,
) {
    use crate::build::ListState as FetchState;
    use crate::ui::SPINNER;

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
                format!("Board (build/ says {} — pick to change)", choice.name)
            }
            crate::build::BoardOrigin::Picked => {
                format!("Board ({}) — pick to change", choice.name)
            }
            crate::build::BoardOrigin::Config => {
                format!(
                    "Board ({}, saved for this project) — pick to change",
                    choice.name
                )
            }
        },
        None => "Board (none set — the first build needs one)".to_string(),
    };

    let [filter_area, hint_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .areas(modal(&title, palette).inner(popup));

    frame.render_widget(modal(&title, palette), popup);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("filter ", muted_style(palette)),
            Span::styled(input.to_string(), Style::new().fg(palette.fg)),
            Span::styled("▏", Style::new().fg(palette.accent)),
        ])),
        filter_area,
    );

    let boards = panel.filtered_boards(input);
    let hint = match &panel.boards.state {
        FetchState::Idle | FetchState::Loading => {
            let spinner = SPINNER[(app.ticks as usize) % SPINNER.len()];
            format!("{spinner} running west boards…")
        }
        FetchState::Failed(error) => error.clone(),
        FetchState::Loaded(_) if boards.is_empty() => "no board matches".to_string(),
        FetchState::Loaded(_) => {
            format!(
                "{} of {} targets (west boards)",
                boards.len(),
                match &panel.boards.state {
                    FetchState::Loaded(all) => all.len(),
                    _ => 0,
                }
            )
        }
    };
    frame.render_widget(
        Paragraph::new(hint.fg(palette.muted)).wrap(ratatui::widgets::Wrap { trim: false }),
        hint_area,
    );

    let items: Vec<ListItem> = boards
        .iter()
        .map(|board| {
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", board.name), Style::new().fg(palette.fg)),
                board.description.clone().fg(palette.muted),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
        list_area,
        &mut state,
    );
}

/// Shield selection: the same filter box over the `west shields` list, with
/// a leading `(none)` row --- the shield is optional, and that row is how an
/// existing answer is cleared (saved with the board, like a board pick).
fn draw_shield_picker(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    input: &str,
    selected: usize,
    palette: Palette,
) {
    use crate::build::ListState as FetchState;
    use crate::ui::SPINNER;

    let height = 20u16;
    let width = 72u16;
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);

    let Some(panel) = app.build.as_ref() else {
        return;
    };

    let title = match &panel.shield {
        Some(name) => format!("Shield ({name}) — pick to change, (none) to clear"),
        None => "Shield (none — optional)".to_string(),
    };

    let [filter_area, hint_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .areas(modal(&title, palette).inner(popup));

    frame.render_widget(modal(&title, palette), popup);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("filter ", muted_style(palette)),
            Span::styled(input.to_string(), Style::new().fg(palette.fg)),
            Span::styled("▏", Style::new().fg(palette.accent)),
        ])),
        filter_area,
    );

    let shields = panel.filtered_shields(input);
    let hint = match &panel.shields.state {
        FetchState::Idle | FetchState::Loading => {
            let spinner = SPINNER[(app.ticks as usize) % SPINNER.len()];
            format!("{spinner} running west shields…")
        }
        FetchState::Failed(error) => error.clone(),
        FetchState::Loaded(_) if shields.is_empty() => "no shield matches".to_string(),
        FetchState::Loaded(_) => {
            format!(
                "{} of {} shields (west shields)",
                shields.len(),
                match &panel.shields.state {
                    FetchState::Loaded(all) => all.len(),
                    _ => 0,
                }
            )
        }
    };
    frame.render_widget(
        Paragraph::new(hint.fg(palette.muted)).wrap(ratatui::widgets::Wrap { trim: false }),
        hint_area,
    );

    // Row 0 is `(none)`: Enter there builds without a shield, which is the
    // answer the optionality of the whole question exists for.
    let mut items = vec![ListItem::new(Line::from(vec![
        Span::styled(" (none) ", Style::new().fg(palette.fg)),
        Span::styled("build without a shield", muted_style(palette)),
    ]))];
    items.extend(shields.iter().map(|shield| {
        ListItem::new(Line::from(vec![
            Span::styled(format!(" {} ", shield.name), Style::new().fg(palette.fg)),
            shield.description.clone().fg(palette.muted),
        ]))
    }));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
        list_area,
        &mut state,
    );
}

/// Serial device selection. Reached automatically when a scan finds more than
/// one board, because guessing would be the wrong kind of convenience.
fn draw_device_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize, palette: Palette) {
    let devices = app.devices.devices();

    if devices.is_empty() {
        let popup = centered(area, 52, 4);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("No MicroPython device found.".fg(palette.warning)),
                Line::from("Connect a board and press 'd' to scan again.".fg(palette.muted)),
            ])
            .block(modal("Device", palette)),
            popup,
        );
        return;
    }

    let items: Vec<ListItem> = devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let mut spans = vec![Span::styled(
                format!(" {} ", device.label()),
                Style::new().fg(palette.fg),
            )];
            if app.devices.selected_index() == Some(index) {
                spans.push(Span::styled("(active)", muted_style(palette)));
            }
            let vid_pid = match device.vendor() {
                Some(vendor) => format!("  {} ({vendor})", device.vid_pid),
                None => format!("  {}", device.vid_pid),
            };
            spans.push(Span::styled(vid_pid, Style::new().fg(palette.accent)));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let popup = centered(area, 64, devices.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal("Device", palette))
            .highlight_style(selection_style(palette)),
        popup,
        &mut state,
    );
}

/// The help overlay: one window, two titled divisions. Navigation is plain
/// rows (its keys move the cursor itself); Commands is the select --- the
/// cursor lands there and `Enter` activates a row. One binding per line, no
/// wrapping: the popup is as wide as the table needs and the descriptions
/// are written to fit it (see `app::help`), truncating with an ellipsis
/// only on terminals narrower than the table. The command list scrolls
/// under its cursor when the terminal is too short for the whole window.
/// The first line is the search field: `/` starts typing, and the filter
/// narrows both divisions (an empty section loses its title too).
fn draw_help(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    filter: &str,
    filtering: bool,
    selected: usize,
    palette: Palette,
) {
    let navigation = help::visible(help::bindings(app.view, HelpSection::Navigation), filter);
    let commands = help::visible(help::bindings(app.view, HelpSection::Commands), filter);

    // Widths come from the whole table, not the filtered subset, so the
    // popup does not jump around as the filter changes.
    let key_col = HelpSection::ALL
        .iter()
        .flat_map(|&section| help::bindings(app.view, section))
        .map(|binding| binding.key.chars().count())
        .max()
        .unwrap_or(0);
    let indent = 2 + key_col + 2;
    let widest = HelpSection::ALL
        .iter()
        .flat_map(|&section| help::bindings(app.view, section))
        .map(
            |binding| indent + binding.description.chars().count() + 2, /* borders */
        )
        .max()
        .unwrap_or(indent + 2);
    let width = widest.min(area.width.into()) as u16;
    let budget = (width as usize).saturating_sub(2 /* borders */ + indent);

    let row = |binding: &crate::app::help::HelpBinding| {
        Line::from(vec![
            Span::styled(
                format!("  {:<width$}  ", binding.key, width = key_col),
                Style::new().fg(palette.accent),
            ),
            Span::styled(
                fit(binding.description, budget),
                Style::new().fg(palette.fg),
            ),
        ])
    };

    // A section that matched nothing is hidden entirely, title included ---
    // the layout below allocates for it only when it renders.
    let visible_titles = usize::from(!navigation.is_empty()) + usize::from(!commands.is_empty());

    let fixed = 1 /* filter line */ + visible_titles /* section titles */ + 2 /* borders */;
    let height = (fixed + navigation.len() + commands.len()) as u16;
    let height = height.min(area.height);
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    let block = modal("Help", palette);
    frame.render_widget(block.clone(), popup);
    let inner = block.inner(popup);

    let inner_width = inner.width as usize;
    let title = |text: &str| {
        Line::from(Span::styled(
            format!(" {text:<width$}", width = inner_width.saturating_sub(1)),
            Style::new()
                .bg(palette.selection)
                .fg(palette.fg)
                .add_modifier(Modifier::BOLD),
        ))
    };

    let filter_line = Line::from(vec![
        Span::styled("filter ", muted_style(palette)),
        Span::styled(filter.to_string(), Style::new().fg(palette.fg)),
        Span::styled(
            if filtering { "▏" } else { " " },
            Style::new().fg(palette.accent),
        ),
        // The way into the search is a key the browsing mode owns, so it
        // rides on the line itself rather than in the footer alone.
        Span::styled(
            if filter.is_empty() && !filtering {
                "  / to search"
            } else {
                ""
            },
            muted_style(palette),
        ),
    ]);

    let mut constraints: Vec<Constraint> = vec![Constraint::Length(1) /* filter line */];
    if !navigation.is_empty() {
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(navigation.len() as u16));
    }
    if !commands.is_empty() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    let areas = Layout::vertical(constraints).split(inner);

    let mut next = 0;
    frame.render_widget(Paragraph::new(filter_line), areas[0]);
    next += 1;
    if !navigation.is_empty() {
        frame.render_widget(
            Paragraph::new(title(HelpSection::Navigation.title())),
            areas[next],
        );
        next += 1;
        frame.render_widget(
            Paragraph::new(
                navigation
                    .iter()
                    .map(|binding| row(binding))
                    .collect::<Vec<_>>(),
            ),
            areas[next],
        );
        next += 1;
    }
    if !commands.is_empty() {
        frame.render_widget(
            Paragraph::new(title(HelpSection::Commands.title())),
            areas[next],
        );
        next += 1;
    }

    let items: Vec<ListItem> = commands
        .iter()
        .map(|binding| ListItem::new(row(binding)))
        .collect();
    let selected = (!commands.is_empty()).then_some(selected.min(commands.len() - 1));
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items).highlight_style(selection_style(palette)),
        areas[next],
        &mut state,
    );
}

/// `text` shortened to `budget` columns with a trailing ellipsis, so a
/// terminal narrower than the help table degrades to a shorter line rather
/// than a wrapped or clipped one.
fn fit(text: &str, budget: usize) -> String {
    if budget == 0 || text.chars().count() <= budget {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(budget - 1).collect();
    shortened.push('…');
    shortened
}

fn draw_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize, palette: Palette) {
    let options = PickerOption::all();
    let active = app.manager.override_kind();

    let items: Vec<ListItem> = options
        .iter()
        .map(|option| {
            let current = match option {
                PickerOption::Automatic => active.is_none(),
                PickerOption::Backend(kind) => active == Some(*kind),
            };
            let mut spans = vec![Span::styled(
                format!(" {} ", option.label()),
                Style::new().fg(palette.fg),
            )];
            if current {
                spans.push(Span::styled("(active)", muted_style(palette)));
            }
            // Detection's own opinion, so an override is an informed choice.
            if let PickerOption::Backend(kind) = option
                && let Some(detection) = app.manager.detection()
            {
                spans.push(Span::styled(
                    format!("  {:.2}", detection.confidence_of(*kind)),
                    Style::new().fg(palette.accent),
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
            .block(modal("Backend", palette))
            .highlight_style(selection_style(palette)),
        popup,
        &mut state,
    );
}

/// The theme picker: every `ratatui_themes::ThemeName`, each row swatched in
/// its own accent color so the pick can be judged before it is applied,
/// rather than by name alone.
fn draw_theme_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize, palette: Palette) {
    let themes = ratatui_themes::ThemeName::all();
    let active = app.theme();

    let items: Vec<ListItem> = themes
        .iter()
        .map(|theme| {
            let swatch = theme.palette().accent;
            let mut spans = vec![
                Span::styled("██ ", Style::new().fg(swatch)),
                Span::styled(
                    format!("{:<16}", theme.display_name()),
                    Style::new().fg(palette.fg),
                ),
            ];
            if *theme == active {
                spans.push(Span::styled("(active)", muted_style(palette)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let popup = centered(area, 44, themes.len() as u16 + 2);
    let mut state = ListState::default().with_selected(Some(selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(modal("Theme", palette))
            .highlight_style(selection_style(palette)),
        popup,
        &mut state,
    );
}

fn draw_sync_preview(
    frame: &mut Frame,
    area: Rect,
    plan: &SyncPlan,
    confirm: bool,
    palette: Palette,
) {
    let mut lines = Vec::new();

    if plan.is_empty() {
        lines.push(Line::from(
            "Nothing to do \u{2014} local and device are in sync.".fg(palette.muted),
        ));
        let height = 7u16;
        draw_confirm_dialog(
            frame,
            area,
            "Sync preview",
            lines,
            confirm,
            (58, height),
            palette,
        );
        return;
    }

    if !plan.uploads.is_empty() {
        lines.push(Line::from(
            format!("Upload {} file(s):", plan.uploads.len())
                .fg(palette.fg)
                .bold(),
        ));
        for (_, device) in plan.uploads.iter().take(8) {
            lines.push(Line::from(format!("  \u{2192} {device}").fg(palette.fg)));
        }
        if plan.uploads.len() > 8 {
            lines.push(Line::from(
                format!("  ... and {} more", plan.uploads.len() - 8).fg(palette.muted),
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
            .fg(palette.fg)
            .bold(),
        ));
        for dir in plan.mkdirs.iter().take(8) {
            lines.push(Line::from(format!("  + {dir}/").fg(palette.fg)));
        }
        if plan.mkdirs.len() > 8 {
            lines.push(Line::from(
                format!("  ... and {} more", plan.mkdirs.len() - 8).fg(palette.muted),
            ));
        }
        lines.push(Line::from(""));
    }

    if !plan.deletes.is_empty() {
        lines.push(Line::from(
            format!("Delete {} device-only file(s):", plan.deletes.len()).fg(palette.warning),
        ));
        for path in plan.deletes.iter().take(8) {
            lines.push(Line::from(format!("  \u{2717} {path}").fg(palette.warning)));
        }
        if plan.deletes.len() > 8 {
            lines.push(Line::from(
                format!("  ... and {} more", plan.deletes.len() - 8).fg(palette.muted),
            ));
        }
    }

    let height = (lines.len() as u16 + 5).min(area.height.saturating_sub(2));
    draw_confirm_dialog(
        frame,
        area,
        "Sync preview",
        lines,
        confirm,
        (60, height),
        palette,
    );
}

fn modal(title: &str, palette: Palette) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette.accent))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
        ))
}

/// Centers a `width`x`height` box inside `area`, shrinking to fit --- shared
/// with the flash dialog (`super::centered`), which sizes itself off its own
/// content the same way every modal here does.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    super::centered(area, width, height)
}
