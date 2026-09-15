//! Consumer-specific routing around the shared path picker.
use super::{App, Overlay};
use crate::path_picker::{FileFilter, PathPicker, PickerKind, PickerOutcome};
use crate::project_config::ProjectConfigRow;
use crate::workspace::DirPurpose;
use ratatui::crossterm::event::KeyEvent;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePurpose {
    Firmware,
    Config(ProjectConfigRow),
}

impl DirPurpose {
    pub fn title(self) -> &'static str {
        match self {
            Self::Installation => "Where is the Zephyr installation?",
            Self::Projects => "Where are your Zephyr projects?",
            Self::MpyProjects => "Where are your MicroPython projects?",
            Self::Install => "Install Zephyr inside this folder (zephyr/)",
            Self::Config(row) => row.label(),
            Self::Project { .. } => "Choose a project folder",
        }
    }

    fn history_key(self) -> String {
        match self {
            Self::Installation => "zephyr_workspace".into(),
            Self::Projects => "zephyr_projects".into(),
            Self::MpyProjects => "micropython_projects".into(),
            Self::Install => "zephyr_install".into(),
            Self::Config(row) => config_history_key(row),
            Self::Project { mpy: true } => "micropython_project".into(),
            Self::Project { mpy: false } => "zephyr_project".into(),
        }
    }
}

fn config_history_key(row: ProjectConfigRow) -> String {
    let (section, key) = row.slot().expect("path rows have configuration slots");
    format!("{section}_{key}")
}

impl App {
    fn picker_start(&self, key: &str, preferred: Option<PathBuf>, fallback: PathBuf) -> PathBuf {
        preferred
            .filter(|path| path.is_dir() || path.is_file())
            .or_else(|| {
                crate::settings::picker_directory(&self.user_config_path(), key, &self.home_dir)
            })
            .unwrap_or(fallback)
    }

    pub(super) fn open_directory_picker(
        &mut self,
        purpose: DirPurpose,
        preferred: Option<PathBuf>,
    ) {
        let fallback = if self.home_dir.is_dir() {
            self.home_dir.clone()
        } else {
            PathBuf::from("/")
        };
        let start = self.picker_start(&purpose.history_key(), preferred, fallback);
        self.overlay = Some(Overlay::DirPicker {
            purpose,
            picker: PathPicker::new(PickerKind::Directory, start, &self.home_dir),
        });
    }

    pub(super) fn directory_picker_error(
        &mut self,
        purpose: DirPurpose,
        path: PathBuf,
        error: String,
    ) {
        let mut picker = match self.overlay.take() {
            Some(Overlay::DirPicker {
                purpose: current,
                picker,
            }) if current == purpose => picker,
            _ => PathPicker::new(PickerKind::Directory, path, &self.home_dir),
        };
        picker.error = Some(error);
        self.overlay = Some(Overlay::DirPicker { purpose, picker });
    }

    fn picker_page(&self) -> usize {
        self.frame_area
            .map(|area| {
                let popup = crate::ui::centered(
                    area,
                    crate::ui::path_picker::WIDTH,
                    crate::ui::path_picker::HEIGHT,
                );
                crate::ui::path_picker::areas(popup)[2].height as usize
            })
            .unwrap_or(10)
    }

    fn remember_picker(&mut self, key: &str, path: &Path) {
        if let Err(err) =
            crate::settings::save_picker_directory(&self.user_config_path(), key, path)
        {
            self.logs.warn(format!(
                "Could not remember picker folder {}: {err}",
                path.display()
            ));
        }
    }

    pub(super) fn on_dir_picker_key(
        &mut self,
        key: KeyEvent,
        purpose: DirPurpose,
        mut picker: PathPicker,
    ) {
        let outcome = picker.handle_key(key, self.picker_page());
        self.overlay = Some(Overlay::DirPicker { purpose, picker });
        match outcome {
            PickerOutcome::Pending => {}
            PickerOutcome::Cancelled => self.return_from_picker(),
            PickerOutcome::Selected(path) => {
                match purpose {
                    DirPurpose::Installation => self.accept_workspace_dir(path.clone()),
                    DirPurpose::Projects => self.accept_projects_dir(path.clone()),
                    DirPurpose::MpyProjects => self.accept_mpy_projects_dir(path.clone()),
                    DirPurpose::Install => self.accept_install_dir(path.clone()),
                    DirPurpose::Config(row) => self.accept_config_path(row, &path),
                    DirPurpose::Project { mpy } => {
                        if !mpy && crate::backend::zephyr::projects::resolve_app(&path).is_none() {
                            self.directory_picker_error(purpose, path,
                                "No Zephyr application here. Choose a folder with find_package(Zephyr) in CMakeLists.txt, or one unambiguous application inside it.".into());
                            return;
                        }
                        if mpy {
                            self.set_mpy_project(path.clone());
                        } else {
                            self.set_project_root(path.clone());
                        }
                        self.overlay = None;
                        self.picker_return = None;
                    }
                }
                if !matches!(
                    self.overlay,
                    Some(Overlay::DirPicker { .. } | Overlay::ConfirmInstallHere { .. })
                ) {
                    self.remember_picker(&purpose.history_key(), &path);
                }
            }
        }
    }

    fn return_from_picker(&mut self) {
        self.overlay = self.picker_return.take();
    }

    pub(super) fn browse_project(&mut self, mpy: bool, dir: Option<PathBuf>) {
        self.picker_return = self.overlay.clone();
        let start = if mpy {
            self.mpy_projects.clone()
        } else {
            self.project_picker_dir(dir.as_deref())
        };
        let purpose = DirPurpose::Project { mpy };
        let start = self.picker_start(
            &purpose.history_key(),
            None,
            start.unwrap_or_else(|| self.project_config_root()),
        );
        self.overlay = Some(Overlay::DirPicker {
            purpose,
            picker: PathPicker::new(PickerKind::Directory, start, &self.home_dir),
        });
    }

    pub(super) fn open_firmware_picker(&mut self) {
        let Some(flash) = &self.flash else {
            return;
        };
        if flash.is_busy() {
            self.logs
                .warn("a command is already running --- stop it first");
            return;
        }
        let fallback = if flash.firmware_dir.is_dir() {
            flash.firmware_dir.clone()
        } else {
            self.project_config_root()
        };
        let start = self.picker_start("firmware", flash.selected_firmware_path(), fallback);
        self.picker_return = None;
        self.overlay = Some(Overlay::FilePicker {
            purpose: FilePurpose::Firmware,
            picker: PathPicker::new(
                PickerKind::File(FileFilter::Firmware),
                start,
                &self.home_dir,
            ),
        });
    }

    pub(super) fn on_file_picker_key(
        &mut self,
        key: KeyEvent,
        purpose: FilePurpose,
        mut picker: PathPicker,
    ) {
        let outcome = picker.handle_key(key, self.picker_page());
        self.overlay = Some(Overlay::FilePicker { purpose, picker });
        match outcome {
            PickerOutcome::Pending => {}
            PickerOutcome::Cancelled => self.return_from_picker(),
            PickerOutcome::Selected(path) => {
                let history = match purpose {
                    FilePurpose::Firmware => {
                        if let Some(flash) = &mut self.flash {
                            flash.choose_firmware(path.clone());
                            if !flash.selected_action().needs_firmware() {
                                flash.set_cursor_to(crate::flash::FlashAction::WriteFlash);
                            }
                            flash.screen = crate::flash::FlashScreen::Options;
                            flash.options_focus = crate::flash::OptionsField::Chip;
                            self.view = super::View::Flash;
                        }
                        self.overlay = None;
                        "firmware".into()
                    }
                    FilePurpose::Config(row) => {
                        self.accept_config_path(row, &path);
                        config_history_key(row)
                    }
                };
                if let Some(parent) = path.parent() {
                    self.remember_picker(&history, parent);
                }
            }
        }
    }

    pub(super) fn open_config_path(&mut self, row: ProjectConfigRow) {
        let Some(kind) = row.picker_kind() else {
            return;
        };
        let Some(panel) = &self.project_config else {
            return;
        };
        let preferred = panel
            .value(row)
            .or_else(|| self.project_config_fallback(row).map(|(value, _)| value))
            .map(|value| {
                let path = crate::settings::expand_home(&value, &self.home_dir);
                if path.is_absolute() {
                    path
                } else {
                    panel.root().join(path)
                }
            });
        let start = self.picker_start(
            &config_history_key(row),
            preferred,
            panel.root().to_path_buf(),
        );
        self.picker_return = Some(Overlay::ProjectConfig);
        let picker = PathPicker::new(kind, start, &self.home_dir);
        self.overlay = Some(match kind {
            PickerKind::Directory => Overlay::DirPicker {
                purpose: DirPurpose::Config(row),
                picker,
            },
            PickerKind::File(_) => Overlay::FilePicker {
                purpose: FilePurpose::Config(row),
                picker,
            },
        });
    }

    fn accept_config_path(&mut self, row: ProjectConfigRow, path: &Path) {
        if let Some(panel) = &mut self.project_config {
            // The application's own relative path should remain portable.
            let value = if row == ProjectConfigRow::ZephyrApp {
                path.strip_prefix(panel.root()).unwrap_or(path)
            } else {
                path
            };
            let value = if value.as_os_str().is_empty() {
                Path::new(".")
            } else {
                value
            };
            panel.set_path(row, value);
        }
        self.return_from_picker();
    }
}
