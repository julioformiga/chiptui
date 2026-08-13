//! Key handling for every modal [`Overlay`], including the shared Yes/No
//! confirm-dialog dispatcher. Split out of `app.rs` since [`App::on_overlay_key`]
//! and its confirm machinery are one cohesive, self-contained concern that
//! reaches into every other subsystem only through `self`.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::BackendKind;

use super::{App, FileAction, Overlay, PendingEdit, PickerOption, ViewerSource};

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
            Overlay::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q')
                ) {
                    self.overlay = None;
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
                    self.apply_picker(selected);
                    self.overlay = None;
                }
                _ => {}
            },
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
                    Self::confirm_flash_action,
                    |app| {
                        if let Some(flash) = &mut app.flash {
                            flash.cancel_pending();
                        }
                    },
                );
            }
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
                        self.apply_project_setup(selected);
                        self.overlay = None;
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
            Overlay::FileActions {
                side,
                name,
                is_dir,
                selected,
            } => {
                let is_text = crate::files::is_text_like(&name);
                let capabilities = self.manager.capabilities();
                let count = FileAction::for_entry(side, is_dir, is_text, capabilities).len();
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
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::FileActions {
                            side,
                            name,
                            is_dir,
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter | KeyCode::Right => {
                        let action =
                            FileAction::for_entry(side, is_dir, is_text, capabilities)[selected];
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
                        // Captured `run` output isn't a file to edit.
                        Some(ViewerSource::RunOutput(_)) | None => {}
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
