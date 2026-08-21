//! Project-pane driving: row 1's checklist rows (the environment questions
//! that moved out of the workspace pane), the shortcuts overlay's way in
//! (`ctrl+k`, then the pane's `e` letter), and the
//! MicroPython half of the project state machine --- the projects folder
//! (`[micropython] projects`, user config) and the session-only project
//! pick that re-roots the file browser's local pane. Split out of `app.rs`
//! alongside the other one-subsystem files.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::Capability;
use crate::workspace::{DirPurpose, WorkspaceAction};

use super::{App, Overlay, ProjectRow};

impl App {
    /// The Project pane's rows: whichever questions the selected backend
    /// asks, in the order they are answered. A workspace-maintaining backend
    /// (Zephyr) asks through [`crate::workspace::WorkspacePanel::actions`];
    /// a project-selecting filesystem backend (MicroPython) asks its four
    /// own. Capability-gated, never backend-kind-gated (`AGENTS.md` §3). A
    /// backend that asks nothing gets no rows --- the pane falls back to
    /// plain detection info.
    pub fn project_rows(&self) -> Vec<ProjectRow> {
        let caps = self.manager.capabilities();
        if let Some(panel) = &self.workspace
            && caps.contains(Capability::WorkspaceSync)
        {
            return panel
                .actions(&caps)
                .into_iter()
                .map(|action| match action {
                    WorkspaceAction::Choose => ProjectRow::ZephyrPath,
                    WorkspaceAction::Projects => ProjectRow::ProjectsBase,
                    WorkspaceAction::Project => ProjectRow::ProjectPath,
                    WorkspaceAction::BoardShield => ProjectRow::BoardShield,
                })
                .collect();
        }
        if caps.contains(Capability::ProjectSelect) && caps.contains(Capability::Filesystem) {
            return vec![
                ProjectRow::MpyProjectsBase,
                ProjectRow::MpyProjectPath,
                ProjectRow::MpyDependencies,
                ProjectRow::MpyScript,
            ];
        }
        Vec::new()
    }

    /// Whether a row's question is still open --- the state the Project
    /// pane's entry cursor lands on (the first one). An *invalid* configured answer
    /// counts as open too: it needs fixing, not celebrating.
    pub(super) fn project_row_open(&self, row: ProjectRow) -> bool {
        match row {
            ProjectRow::ZephyrPath => self
                .workspace
                .as_ref()
                .is_some_and(|panel| panel.resolved.is_none()),
            ProjectRow::ProjectsBase => self
                .workspace
                .as_ref()
                .is_some_and(|panel| panel.projects.is_none()),
            ProjectRow::ProjectPath => !self.project_gate_ok(),
            ProjectRow::BoardShield => self
                .build
                .as_ref()
                .is_some_and(|panel| panel.board.is_none()),
            ProjectRow::MpyProjectsBase => self.mpy_projects.is_none(),
            // The working directory already answers this until a pick
            // overrides it; the dependencies/script rows are reports, not
            // questions.
            ProjectRow::MpyProjectPath | ProjectRow::MpyDependencies | ProjectRow::MpyScript => {
                false
            }
        }
    }

    pub(super) fn first_open_project_row(&self) -> usize {
        self.project_rows()
            .iter()
            .position(|row| self.project_row_open(*row))
            .unwrap_or(0)
    }

    /// Handles a key while the Project pane holds focus: the usual list
    /// grammar over the checklist rows, `Enter` answering the row under the
    /// cursor, and --- on the merged `Board · Shield` row --- `←`/`→`
    /// switching which half the `Enter` acts on (positional: the board is
    /// the left half, the shield the right).
    pub(super) fn on_project_key(&mut self, key: KeyEvent) {
        let rows = self.project_rows();
        let len = rows.len().max(1);
        let current = rows.get(self.project_cursor).copied();
        let mut run: Option<ProjectRow> = None;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.project_cursor = self.project_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.project_cursor = (self.project_cursor + 1).min(len - 1);
            }
            KeyCode::PageUp => self.project_cursor = self.project_cursor.saturating_sub(5),
            KeyCode::PageDown => self.project_cursor = (self.project_cursor + 5).min(len - 1),
            KeyCode::Home => self.project_cursor = 0,
            KeyCode::End => self.project_cursor = len - 1,
            KeyCode::Left if current == Some(ProjectRow::BoardShield) => self.board_segment = true,
            KeyCode::Right if current == Some(ProjectRow::BoardShield) => {
                self.board_segment = false;
            }
            KeyCode::Enter => run = current,
            _ => {}
        }
        if let Some(row) = run {
            self.run_project_row(row);
        }
    }

    /// Answers one checklist row: the Zephyr rows run through the workspace
    /// actions they always did, the MicroPython rows open their own flows,
    /// and the report rows (dependencies, script) carry nothing to run.
    fn run_project_row(&mut self, row: ProjectRow) {
        let action = match row {
            ProjectRow::ZephyrPath => Some(WorkspaceAction::Choose),
            ProjectRow::ProjectsBase => Some(WorkspaceAction::Projects),
            ProjectRow::ProjectPath => Some(WorkspaceAction::Project),
            ProjectRow::BoardShield => Some(WorkspaceAction::BoardShield),
            ProjectRow::MpyProjectsBase => {
                self.open_mpy_projects_dir_picker();
                return;
            }
            ProjectRow::MpyProjectPath => {
                self.open_mpy_project_flow();
                return;
            }
            ProjectRow::MpyDependencies | ProjectRow::MpyScript => return,
        };
        if let Some(action) = action {
            self.run_workspace_action(action);
        }
    }

    // ---- MicroPython projects -------------------------------------------

    /// Resolves `[micropython] projects` (user config only --- a
    /// MicroPython project pins no environment of its own) once per
    /// session. The pickers refresh the answer afterwards; this is only the
    /// startup read.
    pub(super) fn ensure_mpy_projects(&mut self) {
        let caps = self.manager.capabilities();
        if !caps.contains(Capability::ProjectSelect)
            || !caps.contains(Capability::Filesystem)
            || self.mpy_projects_loaded
        {
            return;
        }
        self.mpy_projects_loaded = true;
        if let Some(raw) = crate::settings::mpy_projects_raw(&self.config_dir) {
            let dir = crate::settings::expand_home(&raw, &self.home_dir);
            if dir.is_dir() {
                self.mpy_projects = Some(dir);
            } else {
                let message = format!(
                    "{} does not exist (or is not a directory) — fix [micropython] projects or choose again",
                    dir.display()
                );
                self.logs.error(message.clone());
                self.mpy_projects_invalid = Some(message);
            }
        }
    }

    /// The projects-folder flavor of the directory picker for MicroPython:
    /// same navigation, existence-only validation, persisted to
    /// `[micropython] projects`.
    pub(super) fn open_mpy_projects_dir_picker(&mut self) {
        self.open_purpose_picker(DirPurpose::MpyProjects);
    }

    /// Validates the folder the picker accepted (it only has to exist),
    /// persists it to the user config, and moves straight to the project
    /// picker --- the folder alone browses nothing; the specific project
    /// inside it is the other half of the answer.
    pub(super) fn accept_mpy_projects_dir(&mut self, dir: PathBuf) {
        if !dir.is_dir() {
            self.overlay = Some(Overlay::DirPicker {
                purpose: DirPurpose::MpyProjects,
                path: dir,
                selected: 0,
                error: Some("the folder vanished — it existed when it was accepted".to_string()),
            });
            return;
        }
        let target = self.user_config_path();
        match crate::settings::save_mpy_projects(&target, &dir) {
            Ok(()) => {
                self.logs
                    .info(format!("projects folder saved to {}", target.display()));
            }
            Err(err) => self
                .logs
                .error(format!("could not save {}: {err}", target.display())),
        }
        self.overlay = None;
        self.mpy_projects = Some(dir);
        self.mpy_projects_invalid = None;
        self.open_mpy_project_picker();
    }

    /// Opens whichever picker the MicroPython project question needs next:
    /// the projects folder when none is configured, the project list when
    /// one is.
    pub(super) fn open_mpy_project_flow(&mut self) {
        if self.mpy_projects.is_some() {
            self.open_mpy_project_picker();
        } else {
            self.logs
                .warn("no projects folder configured — where do your MicroPython projects live?");
            self.open_mpy_projects_dir_picker();
        }
    }

    pub(super) fn open_mpy_project_picker(&mut self) {
        self.overlay = Some(Overlay::ProjectPicker {
            mpy: true,
            selected: 0,
            error: None,
        });
    }

    /// Applies the MicroPython project chosen in the picker: session-only,
    /// re-rooting the browser's local pane. Every subdirectory is a
    /// project (no build step to gate on), so nothing is refused.
    pub(super) fn apply_mpy_project_picker(&mut self, selected: usize) {
        let Some(dir) = self.mpy_projects.clone() else {
            self.open_mpy_project_flow();
            return;
        };
        let (rows, read_error) = crate::backend::micropython::projects::project_rows(&dir);
        let Some(row) = rows.get(selected) else {
            let reason = read_error.unwrap_or_else(|| "nothing to pick".to_string());
            self.overlay = Some(Overlay::ProjectPicker {
                mpy: true,
                selected,
                error: Some(reason),
            });
            return;
        };
        self.set_mpy_project(row.path.clone());
        self.logs.info(format!(
            "project set to {} for this session (nothing written)",
            row.path.display()
        ));
        self.overlay = None;
    }

    /// Re-roots the session's MicroPython project: the browser's local pane
    /// follows (both the path and the root its title names), the same
    /// one-fact-two-views rule the Zephyr panes' re-rooting follows.
    pub(super) fn set_mpy_project(&mut self, dir: PathBuf) {
        self.mpy_root = Some(dir.clone());
        if let Some(browser) = &mut self.browser {
            browser.set_local_root(dir);
        }
    }

    /// The MicroPython project root in effect: the session's pick, or the
    /// detection root when none was made. The Dependencies row reads this.
    pub fn mpy_effective_root(&self) -> PathBuf {
        self.mpy_root.clone().unwrap_or_else(|| {
            self.manager.root().map_or_else(
                || self.manager.start_dir().to_path_buf(),
                |root| root.to_path_buf(),
            )
        })
    }

    /// The board's MicroPython version, from whatever REPL/monitor lines
    /// were just seen --- the probe before the first listing, the monitor
    /// afterwards. A banner that says nothing changes nothing.
    pub(super) fn refresh_mpy_version(&mut self, lines: &[String]) {
        if let Some(version) = crate::device::micropython_version(lines)
            && self.mpy_version.as_deref() != Some(version.as_str())
        {
            self.mpy_version = Some(version);
        }
    }
}
