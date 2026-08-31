//! Workspace-pane driving: the action list's key handling, the directory
//! picker (choose the Zephyr installation, validated and persisted), the
//! confirm gate in front of `west update` (which rewrites the shared
//! workspace --- `SPEC.md` §15's never-hide-destruction, applied outside
//! the project). Split out of `app.rs` alongside the other one-subsystem
//! files.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::BackendKind;
use crate::backend::zephyr::projects::{self, ProjectsResolution};
use crate::backend::zephyr::workspace::{Resolution, Workspace, WorkspaceOrigin};
use crate::browser::Side;
use crate::build::BuildAction;
use crate::files;
use crate::workspace::{DirPurpose, WorkspaceAction};

use super::{App, FileAction, Focus, LogTab, MonitorSource, Overlay};

impl App {
    /// Handles a key while [`Focus::Workspace`] holds focus: the project
    /// files list, the whole pane now that the checklist moved up to the
    /// Project pane. The usual list grammar (`j`/`k`, arrows, page,
    /// home/end), `→`/`←`/Backspace descend/ascend directories (pure
    /// navigation, mirroring the file browser's own contract). This pane
    /// has no action menu: `Enter` descends into a directory and opens a
    /// text file straight in `$EDITOR` (a binary or a directory-less
    /// extension does nothing --- there is nothing to open); `v` views a
    /// text file in the viewer; `Del` asks before deleting anything
    /// (default No, [`Overlay::ConfirmDelete`]); `a` creates an entry; `r`
    /// renames the one under the cursor (files and directories alike, via
    /// a pre-filled [`Overlay::RenameEntry`]).
    pub(super) fn on_workspace_key(&mut self, key: KeyEvent) {
        let mut file_action: Option<(String, bool, FileAction)> = None;
        let mut open_create = false;
        let mut open_rename: Option<String> = None;
        // Read before the borrow below: a page is this pane's drawn height
        // (`App::page()`), the rule every scrolling pane follows.
        let page = self.page();

        if let Some(panel) = self.workspace.as_mut() {
            let files_len = panel.files_row_count();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    panel.files_cursor = panel.files_cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if files_len > 0 {
                        panel.files_cursor = (panel.files_cursor + 1).min(files_len - 1);
                    }
                }
                KeyCode::PageUp => panel.files_cursor = panel.files_cursor.saturating_sub(page),
                KeyCode::PageDown => {
                    if files_len > 0 {
                        panel.files_cursor = (panel.files_cursor + page).min(files_len - 1);
                    }
                }
                KeyCode::Home => panel.files_cursor = 0,
                KeyCode::End => panel.files_cursor = files_len.saturating_sub(1),
                KeyCode::Right => panel.enter_files(),
                KeyCode::Left | KeyCode::Backspace => panel.ascend_files(),
                KeyCode::Enter => {
                    if panel.on_parent_row() {
                        // The `[..]` row: Enter steps back up.
                        panel.ascend_files();
                    } else if let Some(entry) = panel.files_selected() {
                        if entry.is_dir {
                            panel.enter_files();
                        } else if files::is_text_like(&entry.name) {
                            file_action = Some((entry.name.clone(), false, FileAction::Edit));
                        }
                        // A binary or otherwise non-editable file does
                        // nothing: there is no menu to fall back on, and
                        // guessing an action would hide that.
                    }
                }
                KeyCode::Char('v') => {
                    if let Some(entry) = panel.files_selected()
                        && !entry.is_dir
                        && files::is_text_like(&entry.name)
                    {
                        file_action = Some((entry.name.clone(), false, FileAction::View));
                    }
                }
                KeyCode::Delete => {
                    if let Some(entry) = panel.files_selected() {
                        file_action = Some((entry.name.clone(), entry.is_dir, FileAction::Delete));
                    }
                }
                KeyCode::Char('a') => open_create = true,
                // Renaming is a *name* change in the listed directory,
                // offered for every entry kind --- a binary's name can
                // change just as well as a text file's or a directory's.
                // The `[..]` parent row is not an entry (`files_selected`
                // returns `None`), so `r` there is a no-op.
                KeyCode::Char('r') => {
                    if let Some(entry) = panel.files_selected() {
                        open_rename = Some(entry.name.clone());
                    }
                }
                _ => {}
            }
        }

        if let Some((name, is_dir, file)) = file_action {
            self.run_file_action(Side::Local, &name, is_dir, file);
        }
        if open_create {
            self.overlay = Some(Overlay::CreateEntry {
                side: Side::Local,
                input: String::new(),
            });
        }
        if let Some(name) = open_rename {
            self.overlay = Some(Overlay::RenameEntry {
                name: name.clone(),
                input: name,
            });
        }
    }

    /// Runs a Project-pane checklist action (the Zephyr rows): every row is
    /// always answerable, so unlike the build panel's lifecycle there is no
    /// disabled case to guard against here --- `Choose`/`Projects` open
    /// their picker, `Project` opens the project flow (warning first when
    /// the current root is not buildable), `BoardShield` opens whichever
    /// half the segment cursor sits on ([`App::board_segment`]).
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
            WorkspaceAction::BoardShield => {
                if self.board_segment {
                    self.open_board_picker();
                } else {
                    self.open_shield_picker();
                }
            }
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

    pub(super) fn open_purpose_picker(&mut self, purpose: DirPurpose) {
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
                    DirPurpose::MpyProjects => self.accept_mpy_projects_dir(path),
                    DirPurpose::Install => self.accept_install_dir(path),
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
    pub(super) fn settings_target(
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
            // The refusal and the way forward arrive together now: the
            // offer states the reason and, declined, puts the picker back
            // exactly as the refusal left it. Before this, a machine with
            // no Zephyr --- or one wanting a *second* installation --- got
            // only the reason and no way on.
            Resolution::Invalid(message) => self.offer_install(dir, message),
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
    pub(crate) fn apply_west_env(&mut self) {
        let Some(workspace) = &self.workspace else {
            return;
        };
        let west_env = workspace.west_env();
        // Which of the two environment rows the action stack offers is the
        // same fact, arriving with the same refresh: `Update Zephyr` once
        // an installation resolves, `Install Zephyr` while none does.
        let installed = workspace.resolved.is_some();
        if let Some(panel) = &mut self.build {
            panel.set_tool_path(west_env.program.clone());
            panel.set_tool_env(west_env.env.clone());
            panel.workspace_installed = installed;
        }
        // The Terminal tab's shell carries this same environment, and a
        // running one cannot be edited from outside: a live session whose
        // birth environment no longer matches is restarted into the new
        // one here, so the tab never quietly disagrees with the commands
        // above. An unchanged environment restarts nothing.
        if self.terminal_process.is_some() && west_env.env != self.terminal_shell_env {
            self.restart_terminal_shell();
        }
    }

    /// Starts a workspace command through the build panel's process slot
    /// (one backend, one running command, whichever pane started it) and
    /// moves the user to where its output streams. `pub(super)` because
    /// `Update` moved to the build panel's action list, but the command
    /// itself stays defined here, next to
    /// [`crate::workspace::WorkspacePanel::update_command`].
    pub(super) fn start_workspace_command(
        &mut self,
        label: &'static str,
        action: BuildAction,
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
            action,
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
        self.start_workspace_command(
            "West update",
            BuildAction::UpdateZephyr,
            |panel, backend| panel.update_command(backend),
        );
    }
}

/// A tool report with the two inputs it was computed from --- see
/// [`App::tool_status`].
pub(super) struct ToolStatusMemo {
    kind: BackendKind,
    located: Vec<(&'static str, std::path::PathBuf)>,
    status: Vec<(&'static str, bool)>,
}

impl App {
    /// Warns about required tools that cannot be run.
    ///
    /// Judging a Zephyr `west` against the inherited `PATH` before the
    /// workspace venv holding it is known would be a false alarm, so every
    /// call site resolves the workspace first --- see
    /// [`Self::ensure_workspace_panel`].
    pub(super) fn report_tools(&mut self) {
        let Some(kind) = self.manager.selected_kind() else {
            return;
        };
        let missing: Vec<&str> = self
            .tool_status()
            .into_iter()
            .filter(|(_, available)| !*available)
            .map(|(tool, _)| tool)
            .collect();

        if !missing.is_empty() {
            self.logs.warn(format!(
                "{kind}: {} not found --- install it to enable the related operations",
                missing.join(", ")
            ));
        }
    }

    /// The selected backend's required tools with whether each is actually
    /// runnable --- the one definition, shared by [`Self::report_tools`]'s
    /// warning and the Project pane's tools row.
    ///
    /// The only thing added here is the *answer* to "did anything resolve a
    /// location for one of these tools": a resolved workspace names the
    /// tools it owns ([`crate::backend::zephyr::workspace::Workspace::tool_locations`])
    /// and the registry judges those files instead of `PATH`. Which tool
    /// that happens to be is the workspace's business, never a branch here.
    ///
    /// Memoized on those two inputs: the render path asks twice a frame and
    /// a miss costs a `PATH` walk per unlocated tool.
    pub fn tool_status(&self) -> Vec<(&'static str, bool)> {
        let Some(kind) = self.manager.selected_kind() else {
            return Vec::new();
        };
        let located = self
            .workspace
            .as_ref()
            .and_then(|panel| panel.resolved.as_ref())
            .map(Workspace::tool_locations)
            .unwrap_or_default();

        let mut cache = self.tool_status_cache.borrow_mut();
        if let Some(memo) = cache.as_ref()
            && memo.kind == kind
            && memo.located == located
        {
            return memo.status.clone();
        }

        let status = self.manager.registry().tool_status(kind, &located);
        *cache = Some(ToolStatusMemo {
            kind,
            located,
            status: status.clone(),
        });
        status
    }
}
