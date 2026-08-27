//! Key handling for every modal [`Overlay`], including the shared Yes/No
//! confirm-dialog dispatcher. Split out of `app.rs` since [`App::on_overlay_key`]
//! and its confirm machinery are one cohesive, self-contained concern that
//! reaches into every other subsystem only through `self`.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::BackendKind;
use crate::browser::Side;
use crate::build::BuildAction;
use crate::device::ScriptState;
use crate::files::SyncStatus;

use super::help::{self, HelpSection};
use super::{
    App, DocsFocus, FileAction, OVERLAY_HELP, PendingEdit, ThemeChoice, View, ViewerSource,
};

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
        // These overlays carry no per-variant hint of their own
        // (`App::shortcuts` returns nothing to guess for them), which
        // otherwise leaves no reachable way to open help while one is up
        // --- `on_key` never lets the dashboard's own `?`/F1 handler see a
        // keystroke while an overlay owns the screen. `F1` always reaches
        // it, since it is never a character a text field could want; `?`
        // joins it everywhere except the three overlays that take free
        // text (`?` typed there must land in the field, not open help).
        if is_help_reachable_overlay(&overlay) {
            let text_entry = is_text_entry_overlay(&overlay);
            if key.code == KeyCode::F(1) || (!text_entry && key.code == KeyCode::Char('?')) {
                self.overlay = Some(OVERLAY_HELP);
                return;
            }
        }
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
            Overlay::BoardPicker {
                input,
                selected,
                scroll,
                focus,
            } => {
                // The list the cursor walks is the *filtered* one, so every
                // filter change re-clamps `selected` against the length the
                // changed filter produces (typing can only shrink it, but
                // backspace grows it too). A new row (filter or cursor)
                // restarts the details pane from its top; only pgup/pgdn
                // and the arrows with the details focused move it
                // deliberately. The keyboard follows `focus` (Tab), while
                // printable keys and `Enter` keep answering the list.
                let rebuild = |app: &mut Self, input: String, mut selected: usize| {
                    if let Some(panel) = app.build.as_ref() {
                        let count = panel.filtered_boards_count(&input);
                        // An empty result leaves row 0 highlighted rather
                        // than an impossible index; `apply` re-checks anyway.
                        selected = selected.min(count.saturating_sub(1));
                    }
                    app.overlay = Some(Overlay::BoardPicker {
                        input,
                        selected,
                        scroll: 0,
                        focus,
                    });
                };
                let count = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_boards_count(&input))
                    .unwrap_or(0)
                    .max(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Backspace => {
                        let mut input = input;
                        input.pop();
                        // A changed filter is a different list --- it starts
                        // from its top, not from wherever the old one sat.
                        self.docs_list_offset = 0;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Char(c) => {
                        let mut input = input;
                        input.push(c);
                        self.docs_list_offset = 0;
                        rebuild(self, input, selected);
                    }
                    // The keyboard follows the focus: with the details
                    // focused the arrows scroll the docs text (clamped by
                    // the renderer, which knows the wrapped length); with
                    // the list focused they walk it. Every printable char
                    // is filter text here either way, including `k`/`j`
                    // (typing "dk" must not move anything).
                    KeyCode::Up | KeyCode::Down if focus == DocsFocus::Details => {
                        let scroll = if key.code == KeyCode::Up {
                            scroll.saturating_sub(1)
                        } else {
                            scroll.saturating_add(1)
                        };
                        self.overlay = Some(Overlay::BoardPicker {
                            input,
                            selected,
                            scroll,
                            focus,
                        });
                    }
                    KeyCode::Up => {
                        let selected = (selected + count - 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Down => {
                        let selected = (selected + 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Tab => {
                        self.overlay = Some(Overlay::BoardPicker {
                            input,
                            selected,
                            scroll,
                            focus: focus.toggled(),
                        });
                    }
                    // The details pane pages by the rows the renderer drew
                    // (`docs_viewport`, the log pane's own contract).
                    KeyCode::PageUp | KeyCode::PageDown => {
                        let page = self.docs_viewport.max(1) as u16;
                        let scroll = if key.code == KeyCode::PageUp {
                            scroll.saturating_sub(page)
                        } else {
                            scroll.saturating_add(page)
                        };
                        self.overlay = Some(Overlay::BoardPicker {
                            input,
                            selected,
                            scroll,
                            focus,
                        });
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.apply_board_picker(&input, selected);
                    }
                    _ => {}
                }
            }
            Overlay::ShieldPicker {
                input,
                selected,
                scroll,
                focus,
            } => {
                // Same grammar as the board picker, over a list whose row 0
                // is the `(none)` row --- the shield is optional, and that
                // row is how it clears.
                let rebuild = |app: &mut Self, input: String, mut selected: usize| {
                    let count = app
                        .build
                        .as_ref()
                        .map(|panel| panel.filtered_shields_count(&input) + 1)
                        .unwrap_or(1);
                    selected = selected.min(count.saturating_sub(1));
                    app.overlay = Some(Overlay::ShieldPicker {
                        input,
                        selected,
                        scroll: 0,
                        focus,
                    });
                };
                let count = self
                    .build
                    .as_ref()
                    .map(|panel| panel.filtered_shields_count(&input) + 1)
                    .unwrap_or(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Backspace => {
                        let mut input = input;
                        input.pop();
                        self.docs_list_offset = 0;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Char(c) => {
                        let mut input = input;
                        input.push(c);
                        self.docs_list_offset = 0;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Up | KeyCode::Down if focus == DocsFocus::Details => {
                        let scroll = if key.code == KeyCode::Up {
                            scroll.saturating_sub(1)
                        } else {
                            scroll.saturating_add(1)
                        };
                        self.overlay = Some(Overlay::ShieldPicker {
                            input,
                            selected,
                            scroll,
                            focus,
                        });
                    }
                    KeyCode::Up => {
                        let selected = (selected + count - 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Down => {
                        let selected = (selected + 1) % count;
                        rebuild(self, input, selected);
                    }
                    KeyCode::Tab => {
                        self.overlay = Some(Overlay::ShieldPicker {
                            input,
                            selected,
                            scroll,
                            focus: focus.toggled(),
                        });
                    }
                    KeyCode::PageUp | KeyCode::PageDown => {
                        let page = self.docs_viewport.max(1) as u16;
                        let scroll = if key.code == KeyCode::PageUp {
                            scroll.saturating_sub(page)
                        } else {
                            scroll.saturating_add(page)
                        };
                        self.overlay = Some(Overlay::ShieldPicker {
                            input,
                            selected,
                            scroll,
                            focus,
                        });
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
            Overlay::Packages => self.on_packages_key(key),
            Overlay::ConfirmRemovePackage {
                name,
                targets,
                declared,
                confirm,
            } => {
                let accepted = (name.clone(), targets.clone());
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmRemovePackage {
                            name,
                            targets,
                            declared,
                            confirm,
                        });
                    },
                    move |app| {
                        let (name, targets) = accepted;
                        // Landed back on the manager *before* removing, not
                        // after: `remove_package`'s own device request can be
                        // gated on a running script, and `check_interrupt_gate`
                        // only replaces `Packages` when it finds it already
                        // showing --- setting this after would let that
                        // dialog open and then get clobbered right back.
                        app.overlay = Some(Overlay::Packages);
                        app.remove_package(&name, &targets, declared);
                    },
                    |app| app.overlay = Some(Overlay::Packages),
                );
            }
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
            // Default *no*, like every other stop/restart: accepting is
            // what lets esptool reset the board into its bootloader to read
            // it. The accept path folds in the interruption a running
            // script would otherwise earn a second question for.
            Overlay::ConfirmIdentifyDevice { confirm } => {
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmIdentifyDevice { confirm });
                    },
                    |app| app.confirm_identify_device(true),
                    |app| app.confirm_identify_device(false),
                );
            }
            // Default *no*, like every other interruption: accepting stops
            // whatever the board is running. Accepting also marks the script
            // stopped --- which releases the held queue --- and arms the
            // restore question for when that queue drains.
            Overlay::ConfirmInterruptDevice {
                confirm,
                return_to_packages,
            } => {
                self.dispatch_confirm(
                    key.code,
                    confirm,
                    move |app, confirm| {
                        app.overlay = Some(Overlay::ConfirmInterruptDevice {
                            confirm,
                            return_to_packages,
                        });
                    },
                    move |app| {
                        app.restore_pending = true;
                        app.set_script_state(ScriptState::Stopped);
                        if return_to_packages {
                            app.overlay = Some(Overlay::Packages);
                        }
                    },
                    move |app| {
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
                        if return_to_packages {
                            app.overlay = Some(Overlay::Packages);
                        }
                    },
                );
            }
            Overlay::ZephyrActions { selected } => {
                const COUNT: usize = 3;
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::ZephyrActions {
                            selected: (selected + COUNT - 1) % COUNT,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::ZephyrActions {
                            selected: (selected + 1) % COUNT,
                        });
                    }
                    KeyCode::Enter if selected == 0 => {
                        self.overlay = Some(Overlay::ConfirmBuild {
                            action: BuildAction::UpdateZephyr,
                            confirm: false,
                        });
                    }
                    KeyCode::Enter if selected == 1 => self.open_sdk_toolchains_shortcut(),
                    // The menu closes before the command starts: the
                    // Monitor tab showing the run must not sit behind a
                    // modal. Routed through `run_build_action` so the
                    // buildable-project gate (the one gate this command
                    // keeps) applies like every other project command.
                    KeyCode::Enter => {
                        self.overlay = None;
                        self.run_build_action(BuildAction::Dashboard);
                    }
                    _ => {}
                }
            }
            Overlay::RestoreDeviceScript {
                selected,
                return_to_packages,
            } => {
                const COUNT: usize = 3;
                let closed = if return_to_packages {
                    Some(Overlay::Packages)
                } else {
                    None
                };
                match key.code {
                    // Dismissing is itself a choice here --- "leave it
                    // stopped" --- so it needs no separate guard.
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = closed,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::RestoreDeviceScript {
                            selected: (selected + COUNT - 1) % COUNT,
                            return_to_packages,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::RestoreDeviceScript {
                            selected: (selected + 1) % COUNT,
                            return_to_packages,
                        });
                    }
                    KeyCode::Enter => {
                        self.overlay = closed;
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

/// Whether `overlay` is one of the "silent" modals --- no hint of its own
/// in `App::shortcuts`, and otherwise no reachable path to help while it is
/// up.
fn is_help_reachable_overlay(overlay: &Overlay) -> bool {
    matches!(
        overlay,
        Overlay::RenameEntry { .. }
            | Overlay::DirPicker { .. }
            | Overlay::BuildDirPicker { .. }
            | Overlay::ProjectPicker { .. }
            | Overlay::DevicePicker { .. }
            | Overlay::ThemePicker { .. }
            | Overlay::FirmwarePicker { .. }
            | Overlay::ProjectSetup { .. }
            | Overlay::FileActions { .. }
            | Overlay::RestoreDeviceScript { .. }
            | Overlay::ZephyrActions { .. }
    )
}

/// The subset of [`is_help_reachable_overlay`] that also takes free-text
/// input --- `?` must land in the field there, so only `F1` may open help.
fn is_text_entry_overlay(overlay: &Overlay) -> bool {
    matches!(
        overlay,
        Overlay::RenameEntry { .. } | Overlay::BuildDirPicker { .. } | Overlay::Packages
    )
}

/// A modal layer drawn above the panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// The help overlay (`?` / F1): one window with both divisions of
    /// [`crate::app::help`] --- Navigation as plain rows, Commands as the
    /// select. `selected` is the cursor among the (filtered) command rows;
    /// `Enter` activates the row by replaying its key after the help
    /// closes. `filter`/`filtering` are the search state --- `/` starts
    /// typing (every printable char is filter text, `j`/`k` included, so
    /// `Esc` returns to the cursor first and closes on the second press),
    /// and the filter narrows both divisions: the dashboard alone lists
    /// twenty-eight rows, so search is the way through them.
    Help {
        filter: String,
        filtering: bool,
        selected: usize,
    },
    /// Serial device selection (`SPEC.md` §8: never guess which board).
    DevicePicker { selected: usize },
    /// The color theme picker (`t`): `Auto` first, then every
    /// `ratatui_themes::ThemeName`, cursor starting on the active choice.
    /// Picking one applies immediately and persists to the user config's
    /// `[ui] theme` (`App::apply_theme_picker`); `Auto` follows the active
    /// backend (Zephyr: Catppuccin Mocha, MicroPython: Everforest) instead
    /// of naming a theme outright.
    ThemePicker { selected: usize },
    /// A destructive esptool action awaiting explicit confirmation
    /// (`SPEC.md` §15). `message` is the literal command about to run, never
    /// a paraphrase.
    Confirm { message: String, confirm: bool },
    /// Firmware file selection when more than one `.bin`/`.elf` was found in
    /// `firmware/`.
    FirmwarePicker { selected: usize },
    /// Empty or unrecognized project: asks which backend this directory is
    /// (`SPEC.md` §7). It fires automatically (detection could not conclude
    /// a backend) and persists the choice to `chiptui.toml`.
    ProjectSetup { selected: usize },
    /// A firmware download would overwrite a file already in `firmware/`;
    /// needs explicit confirmation before running (`SPEC.md` §15 applied to
    /// a filesystem write rather than a device operation).
    ConfirmDownloadOverwrite {
        url: String,
        dest: PathBuf,
        confirm: bool,
    },
    /// Ask for confirmation before uploading a file or directory.
    ConfirmUpload {
        name: String,
        is_dir: bool,
        confirm: bool,
    },
    /// A destructive build-panel action (`Clean`, `Flash`) awaiting explicit
    /// confirmation, showing the literal command (the same rule as the
    /// esptool confirms, `SPEC.md` §15). The message is rebuilt from the
    /// panel state at draw time rather than stored: board and build
    /// directory cannot change while the overlay is open, and this way the
    /// shown command is always the one that would run.
    ConfirmBuild {
        action: crate::build::BuildAction,
        confirm: bool,
    },
    /// The board picker: a filterable `west boards` list, fetched in the
    /// background the first time it opens, enriched from the Zephyr docs
    /// index (picture + detail text for the row under the cursor ---
    /// [`App::docs`]). The boards themselves live in
    /// [`App::build`] ([`crate::build::BuildPanel::boards`]) like the
    /// viewer's content lives in `App::viewer` --- an overlay holds only
    /// what a keypress changes, so rebuilding it per key never re-clones
    /// the list. `input` is the filter text; `scroll` the details pane's
    /// line offset (the arrows with the details focused, pgup/pgdn always);
    /// `focus` which half `Tab` last handed the keyboard ([`DocsFocus`]).
    BoardPicker {
        input: String,
        selected: usize,
        scroll: u16,
        focus: DocsFocus,
    },
    /// The shield picker: the same filterable list grammar over `west
    /// shields`, with a leading `(none)` row --- the shield is optional, and
    /// that row is how an existing pick gets cleared. The list itself lives
    /// in [`App::build`] ([`crate::build::BuildPanel::shields`]) like the
    /// boards do. `input` is the filter text; `scroll` the details pane's
    /// line offset (the arrows with the details focused, pgup/pgdn always);
    /// `focus` which half `Tab` last handed the keyboard ([`DocsFocus`]).
    ShieldPicker {
        input: String,
        selected: usize,
        scroll: u16,
        focus: DocsFocus,
    },
    /// The installation-directory picker: a real filesystem browser (no
    /// discovery guesses --- the user knows where their Zephyr lives).
    /// `error` holds the validation message when an accepted directory
    /// turned out not to be an installation, including the install guide
    /// link; any navigation clears it. `purpose` is which question the
    /// picker answers (installation or projects folder) --- one navigation
    /// component, two validations.
    DirPicker {
        purpose: crate::workspace::DirPurpose,
        path: std::path::PathBuf,
        selected: usize,
        error: Option<String>,
    },
    /// The project picker: the configured projects folder's subdirectories.
    /// For Zephyr (`mpy: false`) each row carries whether it holds build
    /// elements, and accepting a non-buildable directory keeps the picker
    /// open with the reason (`error`) --- a project without a
    /// `CMakeLists.txt` is never built silently. For MicroPython (`mpy:`
    /// `true`) every subdirectory is a project (no build step), so nothing
    /// is marked and nothing is refused. The choice itself is session-only
    /// (`SPEC.md` §10) either way.
    ProjectPicker {
        mpy: bool,
        selected: usize,
        error: Option<String>,
    },
    /// The build-directory picker: the project's configured `build*`
    /// directories plus a typed new name (`west build -d`).
    BuildDirPicker { input: String, selected: usize },
    /// The entry under the cursor in the file browser (`enter`): a small
    /// menu of what to do with it. Which actions show up depends on the pane,
    /// on whether it is a directory, and --- for a file --- whether
    /// [`crate::files::is_text_like`] considers it text --- see
    /// [`FileAction::for_entry`]. The Zephyr workspace pane's embedded file
    /// list never opens this: its keys act directly (see
    /// [`App::run_file_action`]).
    FileActions {
        side: Side,
        name: String,
        is_dir: bool,
        /// The comparison verdict for `name` (`Browser::statuses`), snapshot
        /// when the menu opened so [`FileAction::for_entry`] can offer a
        /// [`FileAction::Diff`] only when the two sides are known to (or might)
        /// differ. `None` when the entry has no comparable status.
        status: Option<SyncStatus>,
        selected: usize,
    },
    /// A file's contents, opened by choosing `View` from [`Overlay::FileActions`]
    /// (`SPEC.md` §2's secondary goal to support external editors, not build
    /// one). Holds no data itself --- [`App::viewer`] does, so scrolling never
    /// re-clones the file the way rebuilding an `Overlay` variant on every
    /// key press would.
    FileViewer,
    /// A device file was edited and just finished re-uploading: offer a
    /// restart so the change actually takes effect, with a btop-style
    /// Yes/No button pair. `confirm` is which one is highlighted --- starts
    /// on `false` (No), unlike every other confirm overlay here, since
    /// restarting interrupts whatever the board is currently doing and
    /// should never happen from a reflex `Enter`.
    ConfirmRestartDevice { confirm: bool },
    /// Ask to flash MicroPython if device is unresponsive.
    ConfirmEraseForMicroPython { confirm: bool },
    /// Ask for confirmation before deleting a file or directory.
    ConfirmDelete {
        side: Side,
        name: String,
        is_dir: bool,
        confirm: bool,
    },
    /// Inline text entry for creating a new entry in `side`'s current
    /// directory (`a`). A trailing `/` on the typed name means "create a
    /// directory" (`SPEC.md` §9's "create directory" action); otherwise an
    /// empty file.
    CreateEntry { side: Side, input: String },
    /// Inline text entry for renaming the entry under the cursor (`r` in the
    /// workspace file list). `name` is the entry's current name, `input` the
    /// edit buffer, pre-filled with it --- editing starts from the end, and
    /// an unedited `Enter` is a no-op, not an error.
    RenameEntry { name: String, input: String },
    /// The package manager (`Enter` or `s` on the Dependencies row, the
    /// device pane's `i`, or the Actions tab's own button): one filterable
    /// list over `requirements.txt`, the board's `/lib` and the
    /// micropython-lib index, fetched through `curl` when it first opens.
    ///
    /// Carries nothing: every field lives on [`App::packages`], so the
    /// remove confirmation --- which *replaces* this overlay, the slot
    /// being one deep --- can hand the window back exactly as it was.
    Packages,
    /// "Remove this package?" --- the manager's `Del`. Its own variant
    /// rather than the shared [`Overlay::Confirm`] (already multiplexed
    /// between the flash panel's `pending` and the installer's start
    /// question), because accepting acts on *two* things and the wording
    /// has to name both.
    ConfirmRemovePackage {
        /// The package name, or the whole specification for a git/URL line.
        name: String,
        /// Paths under `/lib` the removal would delete, with whether each
        /// needs a recursive `rm`. Empty when only the file declares it.
        targets: Vec<(crate::device::DevicePath, bool)>,
        /// Whether `requirements.txt` carries a line for it.
        declared: bool,
        confirm: bool,
    },
    /// A sync plan produced by [`Browser::request_sync`], awaiting the
    /// user's review before execution (`S` in the file browser). Default
    /// is No when the plan includes device-only file deletions, since
    /// deleting is destructive (`SPEC.md` §15).
    SyncPreview {
        plan: crate::browser::SyncPlan,
        confirm: bool,
    },
    /// A device is connected on the selected port, and identifying it ---
    /// reading its chip and firmware over esptool --- **restarts the
    /// board**, stopping whatever it runs. That never happens silently: the
    /// question opens on every device selection (startup scan or picker)
    /// before any identification query or first listing touches the port.
    /// Default is No, like every stop/restart confirm here; declining
    /// leaves the board untouched and the listing proceeds without a
    /// firmware verdict. Answering yes while a script is believed running
    /// accepts that interruption the way
    /// [`Self::ConfirmInterruptDevice`]'s yes does (restore included).
    ConfirmIdentifyDevice { confirm: bool },
    /// Device requests are being held: the app believes a script is running
    /// on the device, and `mpremote` interrupts it (Ctrl-C, then raw REPL)
    /// for every device command --- including the one the user just asked
    /// for. Default is No, like every interruption-confirm here. Accepting
    /// resumes the held queue and arms the restore question for when it
    /// drains; declining drops the queue.
    ///
    /// `return_to_packages` is true when this replaced
    /// [`Self::Packages`] rather than opening over nothing --- the manager
    /// stays the one-deep slot's real content, so answering either way hands
    /// it back instead of leaving the user at a bare dashboard.
    ConfirmInterruptDevice {
        confirm: bool,
        return_to_packages: bool,
    },
    /// Leaving this project for the home screen while commands are still
    /// running: they are cancelled with the session, so the count is named
    /// and the default is No, like every other confirm that loses work.
    ConfirmSwitchProject { confirm: bool },
    /// An interruption the user accepted has finished: how (or whether) to
    /// bring the stopped script back. A three-row picker rather than a
    /// Yes/No, because "restart" has two honest flavors with different
    /// tradeoffs (see [`Self::apply_restore_device_script`]).
    ///
    /// `return_to_packages` mirrors [`Self::ConfirmInterruptDevice`]'s field:
    /// this question can itself replace [`Self::Packages`] when the
    /// interrupt it follows did.
    RestoreDeviceScript {
        selected: usize,
        return_to_packages: bool,
    },
    /// The choice menu behind the `Zephyr Actions` button: update the shared
    /// workspace (`west update`), add SDK toolchains, or generate the build
    /// dashboard (`west build -t dashboard`). Same shape as
    /// [`Self::RestoreDeviceScript`] --- a small list, `j`/`k`/arrows to
    /// move, `Enter` to pick --- except `Esc` here is a plain cancel, not
    /// itself a choice: giving up on "what to run" has no implicit action
    /// the way giving up on "how to restore" does.
    ZephyrActions { selected: usize },
    /// The Zephyr installer: prerequisites, the sequence, and the running
    /// step's output. Carries nothing at all --- every piece of its state
    /// lives on [`App::installer`], which is what lets the panel keep a
    /// process and an output buffer while the overlay value is rebuilt on
    /// each keystroke.
    ZephyrInstall,
    /// The SDK toolchain pick, opened from the installer and returning to
    /// it: the names `west sdk list` reported, multi-selected with space.
    /// An empty pick installs the whole bundle.
    SdkToolchains { selected: usize },
    /// The installation picker refused a directory --- and this is the way
    /// forward from that refusal: install one there, finish a half-built
    /// one, or adopt a complete one sitting in its `zephyr/` subdirectory.
    /// Carries the *picked* folder; the target under it and the wording
    /// are derived at draw time from what is actually there
    /// (`ui::overlay::install_offer`).
    ///
    /// `reason` is the refusal this offer answers. It is shown under the
    /// question *and* is what restores the picker on decline: the overlay
    /// slot is one deep, so an offer that covers the picker has to carry
    /// enough to put it back.
    ///
    /// Its own variant rather than the shared [`Self::Confirm`], whose one
    /// slot is already multiplexed between the flash panel's pending
    /// action and the installer's start question.
    ConfirmInstallHere {
        dir: std::path::PathBuf,
        reason: String,
        confirm: bool,
    },
}
