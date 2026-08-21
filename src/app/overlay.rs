//! Key handling for every modal [`Overlay`], including the shared Yes/No
//! confirm-dialog dispatcher. Split out of `app.rs` since [`App::on_overlay_key`]
//! and its confirm machinery are one cohesive, self-contained concern that
//! reaches into every other subsystem only through `self`.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::BackendKind;
use crate::build::BuildAction;
use crate::device::ScriptState;

use super::help::{self, HelpSection};
use super::{App, FileAction, Overlay, PendingEdit, PickerOption, ThemeChoice, View, ViewerSource};

impl App {
    /// Shared key handling for every Yes/No confirm overlay
    /// (`Overlay::Confirm`, `ConfirmDownloadOverwrite`, `ConfirmUpload`,
    /// `ConfirmRestartDevice`, `ConfirmEraseForMicroPython`,
    /// `ConfirmDelete`): Left/Right/Tab/BackTab/h/l toggle which button is
    /// highlighted, `y`/`n` jump straight to an answer for muscle memory,
    /// `Enter` dispatches on the highlighted button, and `Esc`/`q` decline.
    /// Only `toggle` (rebuilding the overlay --- each variant carries
    /// different associated data) and what `accept`/`decline` actually do
    /// differ per variant; `decline` is a no-op for most of them.
    fn dispatch_confirm(
        &mut self,
        code: KeyCode,
        confirm: bool,
        toggle: impl FnOnce(&mut Self, bool),
        accept: impl FnOnce(&mut Self),
        decline: impl FnOnce(&mut Self),
    ) {
        match code {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('h' | 'l') => toggle(self, !confirm),
            KeyCode::Char('y') => {
                self.overlay = None;
                accept(self);
            }
            KeyCode::Char('n' | 'q') | KeyCode::Esc => {
                self.overlay = None;
                decline(self);
            }
            KeyCode::Enter => {
                self.overlay = None;
                if confirm {
                    accept(self);
                } else {
                    decline(self);
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_overlay_key(&mut self, key: KeyEvent) {
        let Some(overlay) = self.overlay.clone() else {
            return;
        };
        match overlay {
            Overlay::Help {
                filter,
                filtering,
                selected,
            } => {
                // The cursor walks the *filtered* command rows, so every
                // filter change re-clamps `selected` against the length the
                // changed filter produces (typing can only shrink it, but
                // backspace grows it too).
                let count =
                    help::visible(help::bindings(self.view, HelpSection::Commands), &filter)
                        .len()
                        .max(1);
                // Close, then replay the row under the cursor through the
                // screen's own handler --- exactly the event pressing that
                // key outside the help would send. Rows without an event
                // (the toggle itself, plain typing) just close.
                let activate = |app: &mut Self, selected: usize| {
                    let event =
                        help::visible(help::bindings(app.view, HelpSection::Commands), &filter)
                            .get(selected)
                            .and_then(|row| row.event);
                    app.overlay = None;
                    if let Some((code, modifiers)) = event {
                        let event = KeyEvent::new(code, modifiers);
                        match app.view {
                            View::Dashboard => app.on_dashboard_key(event),
                            View::Flash => app.on_flash_key(event),
                        }
                    }
                };
                if filtering {
                    match key.code {
                        // Editing: every printable char is filter text,
                        // `j`/`k` included (typing "dk" must not move the
                        // cursor) --- the rule the board picker set.
                        KeyCode::Char(c) => {
                            let mut text = filter.clone();
                            text.push(c);
                            self.overlay = Some(Overlay::Help {
                                filter: text,
                                filtering: true,
                                selected: selected.min(count.saturating_sub(1)),
                            });
                        }
                        KeyCode::Backspace => {
                            let mut text = filter.clone();
                            text.pop();
                            self.overlay = Some(Overlay::Help {
                                filter: text,
                                filtering: true,
                                selected: selected.min(count.saturating_sub(1)),
                            });
                        }
                        // Esc is the only way out of editing; the second
                        // press closes the window. The filter persists, so
                        // `/` Esc `/` resumes where the search left off.
                        KeyCode::Esc => {
                            self.overlay = Some(Overlay::Help {
                                filter,
                                filtering: false,
                                selected: selected.min(count.saturating_sub(1)),
                            });
                        }
                        KeyCode::Up => {
                            self.overlay = Some(Overlay::Help {
                                filter,
                                filtering: true,
                                selected: (selected + count - 1) % count,
                            });
                        }
                        KeyCode::Down => {
                            self.overlay = Some(Overlay::Help {
                                filter,
                                filtering: true,
                                selected: (selected + 1) % count,
                            });
                        }
                        KeyCode::Enter => activate(self, selected),
                        _ => {}
                    }
                } else {
                    match key.code {
                        // `?` and `q` mirror how the overlay is opened; `/`
                        // starts the search.
                        KeyCode::Esc | KeyCode::Char('?' | 'q') => self.overlay = None,
                        KeyCode::Char('/') => {
                            self.overlay = Some(Overlay::Help {
                                filter,
                                filtering: true,
                                selected: selected.min(count.saturating_sub(1)),
                            });
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.overlay = Some(Overlay::Help {
                                filter,
                                filtering: false,
                                selected: (selected + count - 1) % count,
                            });
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.overlay = Some(Overlay::Help {
                                filter,
                                filtering: false,
                                selected: (selected + 1) % count,
                            });
                        }
                        KeyCode::Home => {
                            self.overlay = Some(Overlay::Help {
                                filter,
                                filtering: false,
                                selected: 0,
                            });
                        }
                        KeyCode::End => {
                            self.overlay = Some(Overlay::Help {
                                filter,
                                filtering: false,
                                selected: count - 1,
                            });
                        }
                        KeyCode::Enter => activate(self, selected),
                        _ => {}
                    }
                }
            }
            Overlay::BackendPicker { selected } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                KeyCode::Up | KeyCode::Char('k') => {
                    let count = PickerOption::all().len();
                    self.overlay = Some(Overlay::BackendPicker {
                        selected: (selected + count - 1) % count,
                    });
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let count = PickerOption::all().len();
                    self.overlay = Some(Overlay::BackendPicker {
                        selected: (selected + 1) % count,
                    });
                }
                KeyCode::Enter => {
                    self.overlay = None;
                    self.apply_picker(selected);
                }
                _ => {}
            },
            Overlay::ThemePicker { selected } => {
                let count = ThemeChoice::all().len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::ThemePicker {
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::ThemePicker {
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.apply_theme_picker(selected);
                    }
                    _ => {}
                }
            }
            Overlay::DevicePicker { selected } => {
                let count = self.devices.devices().len().max(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::DevicePicker {
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::DevicePicker {
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter => {
                        self.apply_device_picker(selected);
                        self.overlay = None;
                    }
                    _ => {}
                }
            }
            Overlay::Confirm { message, confirm } => {
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay = Some(Overlay::Confirm { message, confirm });
                    },
                    // The shared confirm serves two askers; the installer
                    // flags its own so the flash panel's `pending` is not
                    // consulted for a question it never asked.
                    |app| {
                        if app.install_confirm_pending() {
                            app.start_install();
                        } else {
                            app.confirm_flash_action();
                        }
                    },
                    |app| {
                        if app.install_confirm_pending() {
                            app.cancel_install();
                        } else if let Some(flash) = &mut app.flash {
                            flash.cancel_pending();
                        }
                    },
                );
            }
            Overlay::ConfirmInstallHere {
                dir,
                reason,
                confirm,
            } => {
                let accepted = dir.clone();
                let declined = (dir.clone(), reason.clone());
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmInstallHere {
                            dir,
                            reason,
                            confirm,
                        });
                    },
                    move |app| app.open_installer(accepted),
                    move |app| app.decline_install_offer(declined.0, declined.1),
                );
            }
            Overlay::ZephyrInstall => self.on_install_key(key),
            Overlay::SdkToolchains { selected } => self.on_sdk_toolchains_key(key, selected),
            Overlay::FirmwarePicker { selected } => {
                let count = self
                    .flash
                    .as_ref()
                    .map_or(0, |flash| flash.firmware.len())
                    .max(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::FirmwarePicker {
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::FirmwarePicker {
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter => {
                        self.apply_firmware_picker(selected);
                        self.overlay = None;
                    }
                    _ => {}
                }
            }
            Overlay::ProjectSetup { selected } => {
                let count = BackendKind::ALL.len();
                match key.code {
                    // No `q`/esc-cancels-quietly here: leaving this open
                    // means the project stays unrecognized, which is exactly
                    // what re-running detection will ask about again.
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::ProjectSetup {
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::ProjectSetup {
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.apply_project_setup(selected);
                    }
                    _ => {}
                }
            }
            Overlay::ConfirmDownloadOverwrite { url, dest, confirm } => {
                let (accept_url, accept_dest) = (url.clone(), dest.clone());
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay =
                            Some(Overlay::ConfirmDownloadOverwrite { url, dest, confirm });
                    },
                    move |app| app.start_download(accept_url, accept_dest),
                    |_| {},
                );
            }
            Overlay::ConfirmUpload {
                name,
                is_dir,
                confirm,
            } => {
                let accept_name = name.clone();
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmUpload {
                            name,
                            is_dir,
                            confirm,
                        });
                    },
                    move |app| {
                        app.dispatch_browser(|browser, processes, port| {
                            if is_dir {
                                browser.request_upload_dir(&accept_name, processes, port)
                            } else {
                                browser.request_upload(&accept_name, processes, port)
                            }
                        });
                    },
                    |_| {},
                );
            }
            Overlay::ConfirmBuild { action, confirm } => {
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmBuild { action, confirm });
                    },
                    // Accept runs the command directly --- routing back
                    // through `run_build_action` would re-open this confirm.
                    move |app| match action {
                        BuildAction::Build(kind) => app.start_build(kind),
                        BuildAction::Flash => app.start_flash(),
                        BuildAction::UpdateZephyr => app.start_workspace_update(),
                        _ => {}
                    },
                    |_| {},
                );
            }
            Overlay::BoardPicker { input, selected } => {
                // The list the cursor walks is the *filtered* one, so every
                // filter change re-clamps `selected` against the length the
                // changed filter produces (typing can only shrink it, but
                // backspace grows it too).
                let rebuild = |app: &mut Self, input: String, mut selected: usize| {
                    if let Some(panel) = app.build.as_ref() {
                        let count = panel.filtered_boards(&input).len();
                        // An empty result leaves row 0 highlighted rather
                        // than an impossible index; `apply` re-checks anyway.
                        selected = selected.min(count.saturating_sub(1));
                    }
                    app.overlay = Some(Overlay::BoardPicker { input, selected });
                };
                let count = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_boards(&input).len())
                    .unwrap_or(0)
                    .max(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Backspace => {
                        let mut input = input;
                        input.pop();
                        rebuild(self, input, selected);
                    }
                    KeyCode::Char(c) => {
                        let mut input = input;
                        input.push(c);
                        rebuild(self, input, selected);
                    }
                    // Arrows only for navigation: every printable char is
                    // filter text here, including `k`/`j` (typing "dk" must
                    // not move the cursor).
                    KeyCode::Up => {
                        let selected = (selected + count - 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Down => {
                        let selected = (selected + 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.apply_board_picker(&input, selected);
                    }
                    _ => {}
                }
            }
            Overlay::ShieldPicker { input, selected } => {
                // Same grammar as the board picker, over a list whose row 0
                // is the `(none)` row --- the shield is optional, and that
                // row is how it clears.
                let rebuild = |app: &mut Self, input: String, mut selected: usize| {
                    let count = app
                        .build
                        .as_ref()
                        .map(|panel| panel.filtered_shields(&input).len() + 1)
                        .unwrap_or(1);
                    selected = selected.min(count.saturating_sub(1));
                    app.overlay = Some(Overlay::ShieldPicker { input, selected });
                };
                let count = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_shields(&input).len() + 1)
                    .unwrap_or(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Backspace => {
                        let mut input = input;
                        input.pop();
                        rebuild(self, input, selected);
                    }
                    KeyCode::Char(c) => {
                        let mut input = input;
                        input.push(c);
                        rebuild(self, input, selected);
                    }
                    KeyCode::Up => {
                        let selected = (selected + count - 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Down => {
                        let selected = (selected + 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.apply_shield_picker(&input, selected);
                    }
                    _ => {}
                }
            }
            Overlay::DirPicker {
                purpose,
                path,
                selected,
                error,
            } => self.on_dir_picker_key(key, purpose, path, selected, error),
            Overlay::ProjectPicker {
                mpy,
                selected,
                error,
            } => {
                // Same grammar as the other pickers: arrows walk the rows,
                // Enter accepts, Esc leaves. Navigation clears a previous
                // error --- it described a row that is no longer selected.
                // The rows themselves come from whichever projects folder
                // the flavor reads (Zephyr's marked, MicroPython's plain).
                let count = if mpy {
                    self.mpy_projects
                        .as_ref()
                        .map(|dir| {
                            crate::backend::micropython::projects::project_rows(dir)
                                .0
                                .len()
                        })
                        .unwrap_or(0)
                } else {
                    self.workspace
                        .as_ref()
                        .and_then(|panel| panel.projects.as_ref())
                        .map(|dir| crate::backend::zephyr::projects::project_rows(dir).0.len())
                        .unwrap_or(0)
                }
                .max(1);
                let rebuild = |app: &mut Self, selected: usize, error: Option<String>| {
                    app.overlay = Some(Overlay::ProjectPicker {
                        mpy,
                        selected,
                        error,
                    });
                };
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        rebuild(self, (selected + count - 1) % count, None);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        rebuild(self, (selected + 1) % count, None);
                    }
                    KeyCode::Home => rebuild(self, 0, None),
                    KeyCode::End => rebuild(self, count - 1, None),
                    KeyCode::Enter if mpy => self.apply_mpy_project_picker(selected),
                    KeyCode::Enter => self.apply_project_picker(selected),
                    _ => rebuild(self, selected, error),
                }
            }
            Overlay::BuildDirPicker { input, selected } => {
                // Same grammar as the board picker: printable characters are
                // filter/name text, arrows walk, Enter applies.
                let rebuild = |app: &mut Self, input: String, mut selected: usize| {
                    if let Some(panel) = app.build.as_ref() {
                        let count = panel.filtered_build_dirs(&input).len();
                        selected = selected.min(count.saturating_sub(1));
                    }
                    app.overlay = Some(Overlay::BuildDirPicker { input, selected });
                };
                let count = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_build_dirs(&input).len())
                    .unwrap_or(0)
                    .max(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Backspace => {
                        let mut input = input;
                        input.pop();
                        rebuild(self, input, selected);
                    }
                    KeyCode::Char(c) => {
                        let mut input = input;
                        input.push(c);
                        rebuild(self, input, selected);
                    }
                    KeyCode::Up => {
                        let selected = (selected + count - 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Down => {
                        let selected = (selected + 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.apply_build_dir_picker(&input, selected);
                    }
                    _ => {}
                }
            }
            Overlay::FileActions {
                side,
                name,
                is_dir,
                status,
                selected,
            } => {
                let is_text = crate::files::is_text_like(&name);
                let capabilities = self.manager.capabilities();
                let count =
                    FileAction::for_entry(side, is_dir, is_text, status, capabilities).len();
                match key.code {
                    // Left/right mirror the files pane's own navigation
                    // (`←` back, `→` act) so the menu never asks for a
                    // different reflex than the pane it opened from.
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::FileActions {
                            side,
                            name,
                            is_dir,
                            status,
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::FileActions {
                            side,
                            name,
                            is_dir,
                            status,
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter | KeyCode::Right => {
                        let action =
                            FileAction::for_entry(side, is_dir, is_text, status, capabilities)
                                [selected];
                        self.overlay = None;
                        self.run_file_action(side, &name, is_dir, action);
                    }
                    _ => {}
                }
            }
            Overlay::CreateEntry { side, input } => match key.code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Backspace => {
                    let mut input = input;
                    input.pop();
                    self.overlay = Some(Overlay::CreateEntry { side, input });
                }
                KeyCode::Char(c) => {
                    let mut input = input;
                    input.push(c);
                    self.overlay = Some(Overlay::CreateEntry { side, input });
                }
                KeyCode::Enter => {
                    self.overlay = None;
                    self.create_entry(side, &input);
                }
                _ => {}
            },
            // Same grammar as `CreateEntry`: printable characters edit,
            // `Enter` applies, `Esc` leaves the entry exactly as it was.
            Overlay::RenameEntry { name, input } => match key.code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Backspace => {
                    let mut input = input;
                    input.pop();
                    self.overlay = Some(Overlay::RenameEntry { name, input });
                }
                KeyCode::Char(c) => {
                    let mut input = input;
                    input.push(c);
                    self.overlay = Some(Overlay::RenameEntry { name, input });
                }
                KeyCode::Enter => {
                    self.overlay = None;
                    self.rename_entry(&name, &input);
                }
                _ => {}
            },
            Overlay::PackageInstall { input } => match key.code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Backspace => {
                    let mut input = input;
                    input.pop();
                    self.overlay = Some(Overlay::PackageInstall { input });
                }
                KeyCode::Char(c) => {
                    let mut input = input;
                    input.push(c);
                    self.overlay = Some(Overlay::PackageInstall { input });
                }
                KeyCode::Enter => {
                    self.overlay = None;
                    self.install_package(&input);
                }
                _ => {}
            },
            Overlay::FileViewer => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.overlay = None;
                    self.viewer = None;
                }
                KeyCode::Char('e') => {
                    let source = self.viewer.as_ref().map(|viewer| viewer.source.clone());
                    self.overlay = None;
                    self.viewer = None;
                    match source {
                        Some(ViewerSource::Local(path)) => {
                            self.pending_edit = Some(PendingEdit {
                                path,
                                device_target: None,
                            });
                        }
                        Some(ViewerSource::Device(path)) => {
                            let name = path.name().to_string();
                            self.dispatch_browser(|browser, processes, port| {
                                browser.request_edit_download(&name, processes, port)
                            });
                        }
                        // Captured `run` output and a diff view are not a
                        // single file to hand to $EDITOR.
                        Some(ViewerSource::RunOutput(_))
                        | Some(ViewerSource::Diff { .. })
                        | None => {}
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => self.scroll_viewer(-1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll_viewer(1),
                KeyCode::PageUp => self.scroll_viewer(-(self.viewer_viewport.max(1) as isize)),
                KeyCode::PageDown => self.scroll_viewer(self.viewer_viewport.max(1) as isize),
                KeyCode::Home => self.jump_viewer(0),
                KeyCode::End => self.jump_viewer(usize::MAX),
                _ => {}
            },
            // Default *no*: `confirm` starts `false` (No highlighted), so a
            // reflex `Enter` dismisses instead of restarting, unlike every
            // other confirm overlay here --- a restart interrupts whatever
            // the board is doing.
            Overlay::ConfirmRestartDevice { confirm } => {
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmRestartDevice { confirm });
                    },
                    Self::restart_device,
                    |_| {},
                );
            }
            Overlay::ConfirmSwitchProject { confirm } => {
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmSwitchProject { confirm });
                    },
                    Self::request_project_switch,
                    |_| {},
                );
            }
            Overlay::ConfirmEraseForMicroPython { confirm } => {
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmEraseForMicroPython { confirm });
                    },
                    Self::confirm_erase_for_micropython,
                    |_| {},
                );
            }
            Overlay::ConfirmDelete {
                side,
                name,
                is_dir,
                confirm,
            } => {
                let accept_name = name.clone();
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmDelete {
                            side,
                            name,
                            is_dir,
                            confirm,
                        });
                    },
                    move |app| app.delete_file(side, &accept_name, is_dir),
                    |_| {},
                );
            }
            Overlay::SyncPreview { plan, confirm } => {
                let accept_plan = plan.clone();
                let has_deletes = !plan.deletes.is_empty();
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay = Some(Overlay::SyncPreview { plan, confirm });
                    },
                    move |app| {
                        app.dispatch_browser(|browser, processes, port| {
                            browser.execute_sync(&accept_plan, has_deletes, processes, port)
                        });
                    },
                    |_| {},
                );
            }
            // Default *no*, like every other interruption: accepting stops
            // whatever the board is running. Accepting also marks the script
            // stopped --- which releases the held queue --- and arms the
            // restore question for when that queue drains.
            Overlay::ConfirmInterruptDevice { confirm } => {
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmInterruptDevice { confirm });
                    },
                    |app| {
                        app.restore_pending = true;
                        app.set_script_state(ScriptState::Stopped);
                    },
                    |app| {
                        // The listing held behind the identification chain
                        // (a script believed running keeps the chip query
                        // waiting, and the listing waits on the query) is
                        // dropped with the same explainable state. First,
                        // before `dispatch_browser`'s closing gate check
                        // can see it still held and re-ask.
                        app.decline_held_listing();
                        app.dispatch_browser(|browser, _, _| {
                            browser.cancel_held_requests();
                            Vec::new()
                        });
                    },
                );
            }
            Overlay::UpdateZephyrChoice { selected } => {
                const COUNT: usize = 2;
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::UpdateZephyrChoice {
                            selected: (selected + COUNT - 1) % COUNT,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::UpdateZephyrChoice {
                            selected: (selected + 1) % COUNT,
                        });
                    }
                    KeyCode::Enter if selected == 0 => {
                        self.overlay = Some(Overlay::ConfirmBuild {
                            action: BuildAction::UpdateZephyr,
                            confirm: false,
                        });
                    }
                    KeyCode::Enter => self.open_sdk_toolchains_shortcut(),
                    _ => {}
                }
            }
            Overlay::RestoreDeviceScript { selected } => {
                const COUNT: usize = 3;
                match key.code {
                    // Dismissing is itself a choice here --- "leave it
                    // stopped" --- so it needs no separate guard.
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::RestoreDeviceScript {
                            selected: (selected + COUNT - 1) % COUNT,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::RestoreDeviceScript {
                            selected: (selected + 1) % COUNT,
                        });
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.apply_restore_device_script(selected);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Runs the action a [`Overlay::Confirm`] was guarding, once the user
    /// accepted it.
    fn confirm_flash_action(&mut self) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let Some(action) = flash.take_pending() else {
            self.flash = Some(flash);
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let notices = flash.run(action, &mut self.processes, port.as_deref());
        self.flash = Some(flash);
        if notices.is_empty() {
            self.show_flash_in_monitor();
        }
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }
}
