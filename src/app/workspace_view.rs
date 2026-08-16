//! Workspace-pane driving: the action list's key handling, the directory
//! picker (choose the Zephyr installation, validated and persisted), the
//! confirm gate in front of `west update` (which rewrites the shared
//! workspace --- `SPEC.md` §15's never-hide-destruction, applied outside
//! the project). Split out of `app.rs` alongside the other one-subsystem
//! files.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::zephyr::projects::{self, ProjectsResolution};
use crate::backend::zephyr::workspace::{Resolution, WorkspaceOrigin};
use crate::browser::Side;
use crate::workspace::{DirPurpose, WorkspaceAction};

use super::{App, Focus, LogTab, MonitorSource, Overlay};

impl App {
    /// Handles a key while [`Focus::Workspace`] holds focus: one cursor over
    /// two regions of the same pane. While [`crate::workspace::WorkspacePanel::in_files`]
    /// is `false` it walks the checklist with the usual list-navigation
    /// grammar (`j`/`k`, arrows, page, home/end), `Enter` running the action
    /// under the cursor; `Down` past the last checklist row crosses into the
    /// embedded file list instead of wrapping. Once inside the file list,
    /// `Up` at the first entry crosses back; `→`/`←`/Backspace descend/ascend
    /// directories (pure navigation, mirroring the file browser's own
    /// contract); `Enter` opens the file-action menu; `a` creates an entry.
    pub(super) fn on_workspace_key(&mut self, key: KeyEvent) {
        let caps = self.manager.capabilities();
        let mut action = None;
        let mut file_menu: Option<(String, bool)> = None;
        let mut open_create = false;

        if let Some(panel) = self.workspace.as_mut() {
            let checklist_len = panel.actions(&caps).len().max(1);
            if !panel.in_files {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        panel.cursor = panel.cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if panel.cursor + 1 >= checklist_len {
                            panel.in_files = true;
                            panel.files_cursor = 0;
                        } else {
                            panel.cursor += 1;
                        }
                    }
                    KeyCode::PageUp => panel.cursor = panel.cursor.saturating_sub(5),
                    KeyCode::PageDown => panel.cursor = (panel.cursor + 5).min(checklist_len - 1),
                    KeyCode::Home => panel.cursor = 0,
                    KeyCode::End => panel.cursor = checklist_len - 1,
                    KeyCode::Enter => action = panel.action_at(&caps, panel.cursor),
                    _ => {}
                }
            } else {
                let files_len = panel.files_entries.len();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        if panel.files_cursor == 0 {
                            panel.in_files = false;
                            panel.cursor = checklist_len - 1;
                        } else {
                            panel.files_cursor -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if files_len > 0 {
                            panel.files_cursor = (panel.files_cursor + 1).min(files_len - 1);
                        }
                    }
                    KeyCode::PageUp => panel.files_cursor = panel.files_cursor.saturating_sub(5),
                    KeyCode::PageDown => {
                        if files_len > 0 {
                            panel.files_cursor = (panel.files_cursor + 5).min(files_len - 1);
                        }
                    }
                    KeyCode::Home => panel.files_cursor = 0,
                    KeyCode::End => panel.files_cursor = files_len.saturating_sub(1),
                    KeyCode::Right => panel.enter_files(),
                    KeyCode::Left | KeyCode::Backspace => panel.ascend_files(),
                    KeyCode::Enter => {
                        if let Some(entry) = panel.files_selected() {
                            file_menu = Some((entry.name.clone(), entry.is_dir));
                        }
                    }
                    KeyCode::Char('a') => open_create = true,
                    _ => {}
                }
            }
        }

        if let Some(action) = action {
            self.run_workspace_action(action);
        }
        if let Some((name, is_dir)) = file_menu {
            self.overlay = Some(Overlay::FileActions {
                side: Side::Local,
                name,
                is_dir,
                status: None,
                selected: 0,
            });
        }
        if open_create {
            self.overlay = Some(Overlay::CreateEntry {
                side: Side::Local,
                input: String::new(),
            });
        }
    }

    /// Runs a checklist action: every row is always answerable, so unlike
    /// the build panel's lifecycle there is no disabled case to guard
    /// against here --- `Choose`/`Projects`/`Board`/`Shield` open their
    /// picker, `Project` opens the project flow (warning first when the
    /// current root is not buildable).
    pub(super) fn run_workspace_action(&mut self, action: WorkspaceAction) {
        match action {
            WorkspaceAction::Choose => self.open_dir_picker(),
            WorkspaceAction::Projects => self.open_projects_dir_picker(),
            WorkspaceAction::Project => {
                // The checklist row doubles as the gate's explanation: a
                // root without build elements says so before the flow that
                // answers it opens.
                if !self.project_gate_ok()
                    && let Some(panel) = &self.build
                {
                    let root = panel.root.display().to_string();
                    self.logs.warn(format!(
                        "{root} is not a Zephyr application (no CMakeLists.txt) — pick a project first"
                    ));
                }
                self.open_project_flow();
            }
            WorkspaceAction::Board => self.open_board_picker(),
            WorkspaceAction::Shield => self.open_shield_picker(),
        }
    }

    /// Startup's half of the flow (`SPEC.md` §10's environment): when no
    /// config names an installation, the question is asked right away, not
    /// parked in a pane waiting for a keypress. Called from `main.rs` after
    /// the panes exist. A config that names an *invalid* location does not
    /// reopen the picker by itself --- its error (with the install guide)
    /// is the pane's status, and the chooser is one `Enter` away.
    pub fn maybe_open_workspace_picker(&mut self) {
        let not_configured = self
            .workspace
            .as_ref()
            .is_some_and(|panel| panel.resolved.is_none() && panel.invalid.is_none());
        if not_configured && self.overlay.is_none() {
            self.open_dir_picker();
        }
    }

    /// Opens the directory picker at the user's home (where installations
    /// live in practice), or `/` when there is no home to start from.
    /// `Esc` leaves everything as it was.
    pub(super) fn open_dir_picker(&mut self) {
        self.open_purpose_picker(DirPurpose::Installation);
    }

    /// The projects-folder flavor of the same picker: same navigation,
    /// existence-only validation, persisted to `[zephyr] projects`. The
    /// accepted folder feeds the project picker, which opens right after
    /// the save --- the two answers are one question ("what am I
    /// building?") asked in order.
    pub(super) fn open_projects_dir_picker(&mut self) {
        self.open_purpose_picker(DirPurpose::Projects);
    }

    fn open_purpose_picker(&mut self, purpose: DirPurpose) {
        let start = if self.home_dir.is_dir() {
            self.home_dir.clone()
        } else {
            PathBuf::from("/")
        };
        self.overlay = Some(Overlay::DirPicker {
            purpose,
            path: start,
            selected: 0,
            error: None,
        });
    }

    /// Applies a key to the open directory picker: arrows walk the rows,
    /// `Enter` (or `→`) opens the row under the cursor --- descending into
    /// directories, stepping up at `..`, and *validating* the current
    /// directory at the "use this directory" row --- and `←`/Backspace go
    /// up. Any navigation clears a previous validation error: it described
    /// a directory that is no longer under the cursor.
    pub(super) fn on_dir_picker_key(
        &mut self,
        key: KeyEvent,
        purpose: DirPurpose,
        path: PathBuf,
        selected: usize,
        error: Option<String>,
    ) {
        let (rows, _) = crate::workspace::dir_rows(&path);
        let count = rows.len().max(1);
        let rebuild = |app: &mut Self, path: PathBuf, selected: usize| {
            let (rows, _) = crate::workspace::dir_rows(&path);
            let selected = selected.min(rows.len().saturating_sub(1));
            app.overlay = Some(Overlay::DirPicker {
                purpose,
                path,
                selected,
                error: None,
            });
        };
        let descend = |app: &mut Self, path: PathBuf| {
            // Landing on the "use this directory" row is the point: the
            // reflex Enter after navigating *into* the right directory
            // accepts it, instead of asking for one more hop.
            app.overlay = Some(Overlay::DirPicker {
                purpose,
                path,
                selected: 0,
                error: None,
            });
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
            KeyCode::Up | KeyCode::Char('k') => {
                rebuild(self, path, (selected + count - 1) % count);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                rebuild(self, path, (selected + 1) % count);
            }
            KeyCode::Enter | KeyCode::Right => match rows.get(selected).map(|row| row.kind) {
                Some(crate::workspace::DirRowKind::Use) => match purpose {
                    DirPurpose::Installation => self.accept_workspace_dir(path),
                    DirPurpose::Projects => self.accept_projects_dir(path),
                },
                Some(crate::workspace::DirRowKind::Parent) => {
                    let Some(parent) = path.parent().map(Path::to_path_buf) else {
                        return;
                    };
                    descend(self, parent);
                }
                Some(crate::workspace::DirRowKind::Dir) => {
                    let Some(dir) = rows.get(selected).map(|row| row.path.clone()) else {
                        return;
                    };
                    descend(self, dir);
                }
                None => rebuild(self, path, selected),
            },
            KeyCode::Left | KeyCode::Backspace => {
                let Some(parent) = path.parent().map(Path::to_path_buf) else {
                    return;
                };
                descend(self, parent);
            }
            _ => {
                if error.is_some() {
                    self.overlay = Some(Overlay::DirPicker {
                        purpose,
                        path,
                        selected,
                        error,
                    });
                }
            }
        }
    }

    /// Validates the directory the picker accepted and, when it is a real
    /// Zephyr installation, persists the choice where resolution reads it:
    /// the project's `chiptui.toml` when the project pins its own location,
    /// else the user config. A directory that fails validation keeps the
    /// picker open with the reason (and the install guide) under the list.
    /// Which config file an answered environment question is written to: a
    /// project that pins its own installation keeps both keys together in
    /// its `chiptui.toml`, everyone else answers once, machine-wide. Shared
    /// by both accept paths so the two halves of the environment can never
    /// end up in different files.
    fn settings_target(
        &self,
        root: &Path,
        project_settings: Option<&crate::settings::ZephyrSettings>,
    ) -> PathBuf {
        if project_settings.is_some_and(|settings| settings.workspace.is_some()) {
            root.join(crate::project::config::FILE_NAME)
        } else {
            self.user_config_path()
        }
    }

    pub(super) fn accept_workspace_dir(&mut self, dir: PathBuf) {
        let (root, project_settings, user_settings) = self.zephyr_settings();
        // The picker validates through whatever explicit west/sdk keys the
        // configs carry, so a pick honors them the same way a resolved
        // location would.
        let settings = project_settings
            .clone()
            .or_else(|| user_settings.clone())
            .unwrap_or_default();
        let input = crate::backend::zephyr::workspace::ResolveInput {
            project_settings: project_settings.as_ref(),
            user_settings: user_settings.as_ref(),
            home: &self.home_dir,
        };
        let checked = crate::backend::zephyr::workspace::install_check(
            &input,
            dir.clone(),
            WorkspaceOrigin::UserConfig,
            &settings,
        );
        match checked {
            Resolution::Single(_) => {
                let target = self.settings_target(&root, project_settings.as_ref());
                match crate::settings::save_workspace(&target, &dir) {
                    Ok(()) => {
                        self.logs
                            .info(format!("Zephyr location saved to {}", target.display()));
                    }
                    Err(err) => self
                        .logs
                        .error(format!("could not save {}: {err}", target.display())),
                }
                self.overlay = None;
                self.refresh_workspace_resolution();
            }
            Resolution::Invalid(message) => {
                self.overlay = Some(Overlay::DirPicker {
                    purpose: DirPurpose::Installation,
                    path: dir,
                    selected: 0,
                    error: Some(message),
                });
            }
            Resolution::NotConfigured => {}
        }
    }

    /// Validates the projects folder the picker accepted (it only has to
    /// exist), persists it beside the installation key, and moves straight
    /// to the project picker --- the folder alone builds nothing; the
    /// specific project inside it is the other half of the answer.
    pub(super) fn accept_projects_dir(&mut self, dir: PathBuf) {
        match projects::dir_check(dir.clone()) {
            ProjectsResolution::Configured(_) => {
                let (root, project_settings, _user_settings) = self.zephyr_settings();
                let target = self.settings_target(&root, project_settings.as_ref());
                match crate::settings::save_projects(&target, &dir) {
                    Ok(()) => {
                        self.logs
                            .info(format!("projects folder saved to {}", target.display()));
                    }
                    Err(err) => self
                        .logs
                        .error(format!("could not save {}: {err}", target.display())),
                }
                self.overlay = None;
                self.refresh_workspace_resolution();
                self.open_project_picker();
            }
            ProjectsResolution::Invalid(message) => {
                self.overlay = Some(Overlay::DirPicker {
                    purpose: DirPurpose::Projects,
                    path: dir,
                    selected: 0,
                    error: Some(message),
                });
            }
            ProjectsResolution::NotConfigured => {}
        }
    }

    /// Re-runs resolution from the (possibly just-written) configs and
    /// pushes the answers into the pane and the build panel's commands.
    /// Both environment facts refresh together: they live in the same
    /// config section and were saved by the same kind of picker.
    pub(super) fn refresh_workspace_resolution(&mut self) {
        let resolution = self.resolve_workspace();
        let projects_resolution = self.resolve_projects();
        if let Resolution::Invalid(message) = &resolution {
            self.logs.error(message.clone());
        }
        if let ProjectsResolution::Invalid(message) = &projects_resolution {
            self.logs.error(message.clone());
        }
        if let Some(panel) = &mut self.workspace {
            panel.apply_resolution(resolution);
            panel.apply_projects(projects_resolution);
        }
        self.apply_west_env();
    }

    /// Pushes the resolved workspace's west invocation (executable and
    /// environment) into the build panel, whose commands are where it
    /// matters.
    pub(super) fn apply_west_env(&mut self) {
        let Some(workspace) = &self.workspace else {
            return;
        };
        let west_env = workspace.west_env();
        if let Some(panel) = &mut self.build {
            panel.set_tool_path(west_env.program.clone());
            panel.set_tool_env(west_env.env);
        }
    }

    /// Starts a workspace command through the build panel's process slot
    /// (one backend, one running command, whichever pane started it) and
    /// moves the user to where its output streams. `pub(super)` since
    /// [`super::build_view`]'s `SdkList` action calls it too --- both
    /// `Update`/`SdkList` moved to the build panel's action list, but the
    /// commands themselves stay defined here, next to
    /// [`crate::workspace::WorkspacePanel::update_command`]/`sdk_list_command`.
    pub(super) fn start_workspace_command(
        &mut self,
        label: &'static str,
        command: impl FnOnce(
            &mut crate::workspace::WorkspacePanel,
            &dyn crate::backend::Backend,
        ) -> Option<crate::process::Command>,
    ) {
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(workspace) = &mut self.workspace else {
            return;
        };
        let Some(command) = command(workspace, backend) else {
            self.logs
                .warn(format!("{label}: resolve a workspace first"));
            return;
        };
        let Some(panel) = &mut self.build else {
            return;
        };
        if panel.is_busy() {
            self.logs.warn("a build command is already running");
            return;
        }
        if !panel.start(
            label,
            false,
            crate::build::Follow::Keep,
            command,
            &mut self.processes,
            &self.manager.capabilities(),
        ) {
            return;
        }
        self.view = super::View::Dashboard;
        self.focus = Focus::Logs;
        self.log_tab = LogTab::Monitor;
        self.set_monitor_source(MonitorSource::Build);
    }

    /// The confirm overlay's accept path for `west update`.
    pub(super) fn start_workspace_update(&mut self) {
        self.start_workspace_command("West update", |panel, backend| {
            panel.update_command(backend)
        });
    }
}
