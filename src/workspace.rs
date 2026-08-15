//! The workspace pane: the Zephyr backend's *environment* state machine,
//! the row-2 counterpart of [`crate::build::BuildPanel`].
//!
//! Where the build panel owns what happens *in the project* (build, flash,
//! board), this one owns what is *shared across projects*: which Zephyr
//! installation the commands run against, which `west` executable (the
//! workspace's venv, when it has one), which SDK --- plus the two
//! workspace-scoped operations, `west update` and `west sdk list`
//! (`Capability::WorkspaceSync`).
//!
//! The location is never guessed. It comes from configuration
//! ([`crate::backend::zephyr::workspace::resolve`]); when no config names
//! it, the pane's `Choose` action (and startup itself) opens the directory
//! picker, whose answer is validated by the same rules and then persisted
//! by [`crate::settings::save_workspace`] --- so the config file remains
//! the single source of truth, as `SPEC.md` §13 keeps it.
//!
//! Like every panel here it is a pure state machine: running commands is
//! delegated to the build panel's single process slot --- one backend, one
//! running command, whatever pane started it.

use std::path::{Path, PathBuf};

use crate::backend::Capability;
use crate::backend::zephyr::projects::ProjectsResolution;
use crate::backend::zephyr::workspace::{Resolution, Workspace};
use crate::process::Command;

/// One row of the workspace pane's action list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAction {
    /// `west update` --- syncs the manifest's projects into the workspace.
    /// Slow, network-bound and rewrites the workspace, so the app confirms
    /// it with the literal command before running (`SPEC.md` §15).
    Update,
    /// `west sdk list` --- the toolchain inventory. Read-only.
    SdkList,
    /// Opens the directory picker over the filesystem: the user knows
    /// where their installation lives, and the pick is validated then
    /// saved to the config (`SPEC.md` §8's never-guess rule, applied to
    /// the environment).
    Choose,
    /// Opens the directory picker for the *projects* folder: where the
    /// user's Zephyr applications live. Same never-guess rule, same
    /// validate-then-save path --- answered before any project command
    /// needs it (see [`crate::backend::Capability::ProjectSelect`]).
    Projects,
    /// Chooses which *project* the lifecycle runs in (the build panel's
    /// root): the picker lists the configured projects folder's
    /// subdirectories, and the pick re-roots every command (session-only).
    /// The build panel's answers are asked here --- beside the other
    /// prerequisites --- and answered there. Under
    /// [`crate::backend::Capability::ProjectSelect`].
    Project,
    /// Opens the board picker (the build panel's board answer, asked
    /// beside the other prerequisites). Under
    /// [`crate::backend::Capability::BoardSelect`].
    Board,
    /// Opens the shield picker (the build panel's optional shield answer,
    /// asked right under the board it rides on). A pick is session-only
    /// like a board one, and the picker's `(none)` row is how it clears.
    /// Under [`crate::backend::Capability::ShieldSelect`].
    Shield,
}

pub struct WorkspacePanel {
    /// The current answer, once resolution or a saved pick produced one.
    /// `None` while unresolved (not configured, or an invalid location).
    pub resolved: Option<Workspace>,
    /// Why a configured location failed validation, when one did
    /// (`Resolution::Invalid`). Includes the install guide link.
    pub invalid: Option<String>,
    /// The configured projects folder, once resolution or a saved pick
    /// produced one (the environment's second persisted fact --- the
    /// build panel's project picker lists its subdirectories).
    pub projects: Option<PathBuf>,
    /// Why a configured projects folder failed validation, when one did.
    pub projects_invalid: Option<String>,
    pub cursor: usize,
    /// The inherited `PATH` at startup, baked into every derived
    /// [`crate::backend::zephyr::workspace::WestEnv`] (the venv's `bin` is
    /// prepended to it, never replacing it).
    path_env: String,
}

impl WorkspacePanel {
    /// Builds the pane from a resolution. `path_env` is the process's own
    /// `PATH` (an empty string is fine --- only venv workspaces prepend
    /// anything to it).
    pub fn new(resolution: Resolution, path_env: impl Into<String>) -> Self {
        let mut panel = Self {
            resolved: None,
            invalid: None,
            projects: None,
            projects_invalid: None,
            cursor: 0,
            path_env: path_env.into(),
        };
        panel.apply_resolution(resolution);
        panel
    }

    /// Applies a fresh resolution (after the picker saved a location, or
    /// the config changed on disk). The new answer always wins: there is no
    /// session pick to preserve, because a pick only ever counts once it is
    /// written to a config file.
    pub fn apply_resolution(&mut self, resolution: Resolution) {
        match resolution {
            Resolution::Single(workspace) => {
                self.resolved = Some(workspace);
                self.invalid = None;
            }
            Resolution::Invalid(message) => {
                self.invalid = Some(message);
                self.resolved = None;
            }
            Resolution::NotConfigured => {
                self.resolved = None;
                self.invalid = None;
            }
        }
    }

    /// The west invocation for the resolved workspace (or the bare `west`
    /// fallback while unresolved, matching the pre-workspace behavior).
    pub fn west_env(&self) -> crate::backend::zephyr::workspace::WestEnv {
        self.resolved.as_ref().map_or_else(
            crate::backend::zephyr::workspace::WestEnv::from_path,
            |workspace| workspace.to_env(&self.path_env),
        )
    }

    /// Applies a fresh projects-folder resolution, independent of the
    /// installation's: the two settings answer different questions and can
    /// be fixed separately.
    pub fn apply_projects(&mut self, resolution: ProjectsResolution) {
        match resolution {
            ProjectsResolution::Configured(dir) => {
                self.projects = Some(dir);
                self.projects_invalid = None;
            }
            ProjectsResolution::Invalid(message) => {
                self.projects_invalid = Some(message);
                self.projects = None;
            }
            ProjectsResolution::NotConfigured => {
                self.projects = None;
                self.projects_invalid = None;
            }
        }
    }

    /// Rows the action list shows: the checklist questions first (the
    /// installation, the projects folder, the project, the board and its
    /// optional shield --- the prerequisites in the order they are
    /// answered), then the workspace operations as buttons that stay
    /// visible but dim until the installation is resolved.
    pub fn actions(&self, caps: &crate::backend::Capabilities) -> Vec<WorkspaceAction> {
        if !caps.contains(Capability::WorkspaceSync) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        actions.push(WorkspaceAction::Choose);
        if caps.contains(Capability::ProjectSelect) {
            actions.push(WorkspaceAction::Projects);
            actions.push(WorkspaceAction::Project);
        }
        if caps.contains(Capability::BoardSelect) {
            actions.push(WorkspaceAction::Board);
        }
        if caps.contains(Capability::ShieldSelect) {
            actions.push(WorkspaceAction::Shield);
        }
        actions.push(WorkspaceAction::Update);
        actions.push(WorkspaceAction::SdkList);
        actions
    }

    /// Whether `Enter` runs the action: the choosers always answer (they
    /// are also how to *change* an answer later), while the operations need
    /// a resolved installation --- a disabled button is dimmed, never
    /// hidden, so the checklist state explains itself.
    pub fn action_enabled(&self, action: WorkspaceAction) -> bool {
        match action {
            WorkspaceAction::Choose
            | WorkspaceAction::Projects
            | WorkspaceAction::Project
            | WorkspaceAction::Board
            | WorkspaceAction::Shield => true,
            WorkspaceAction::Update | WorkspaceAction::SdkList => self.resolved.is_some(),
        }
    }

    pub fn action_at(
        &self,
        caps: &crate::backend::Capabilities,
        index: usize,
    ) -> Option<WorkspaceAction> {
        self.actions(caps).into_iter().nth(index)
    }

    /// `west update`, run *in the workspace* (its own root, not the
    /// project: the operation's subject) with the resolved environment.
    /// `None` without a resolved workspace or a supporting backend.
    pub fn update_command(&self, backend: &dyn crate::backend::Backend) -> Option<Command> {
        let workspace = self.resolved.as_ref()?;
        Some(
            self.west_env()
                .apply(backend.workspace_update_command()?)
                .current_dir(&workspace.dir),
        )
    }

    /// `west sdk list`, same rooting and environment as [`Self::update_command`].
    pub fn sdk_list_command(&self, backend: &dyn crate::backend::Backend) -> Option<Command> {
        let workspace = self.resolved.as_ref()?;
        Some(
            self.west_env()
                .apply(backend.sdk_list_command()?)
                .current_dir(&workspace.dir),
        )
    }

    /// The installation's location for status display, when one is resolved.
    pub fn dir(&self) -> Option<&PathBuf> {
        self.resolved.as_ref().map(|workspace| &workspace.dir)
    }
}

/// One navigable row of the directory picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirRow {
    pub name: String,
    pub path: PathBuf,
    pub kind: DirRowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirRowKind {
    /// Accepts the current directory as the chosen location.
    Use,
    /// Steps up to the parent.
    Parent,
    /// Descends into the named subdirectory.
    Dir,
}

/// Which question the directory picker is answering. The navigation is one
/// component; only the accept-path validation and the title differ --- a
/// directory is an *installation* when `.west/` says so, a *projects
/// folder* when it simply exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirPurpose {
    /// "Where is the Zephyr installation?" --- validated by
    /// [`crate::backend::zephyr::workspace::install_check`].
    Installation,
    /// "Where are your Zephyr projects?" --- validated by existence only;
    /// the build-element test belongs to the projects inside it
    /// ([`crate::backend::zephyr::projects`]).
    Projects,
}

/// The directory picker's rows for `path`: "use this directory" first (the
/// target of a reflex `Enter` when the user has navigated to the right
/// place), the parent when one exists, then every subdirectory sorted by
/// name. A directory that cannot be read still lists the first rows and
/// reports why, so navigation never dead-ends.
pub fn dir_rows(path: &Path) -> (Vec<DirRow>, Option<String>) {
    let mut rows = vec![DirRow {
        name: "use this directory".to_string(),
        path: path.to_path_buf(),
        kind: DirRowKind::Use,
    }];
    if let Some(parent) = path.parent() {
        rows.push(DirRow {
            name: "..".to_string(),
            path: parent.to_path_buf(),
            kind: DirRowKind::Parent,
        });
    }
    match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut dirs: Vec<(String, PathBuf)> = entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .map(|entry| {
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.path(),
                    )
                })
                .collect();
            dirs.sort();
            for (name, path) in dirs {
                rows.push(DirRow {
                    name,
                    path,
                    kind: DirRowKind::Dir,
                });
            }
            (rows, None)
        }
        Err(err) => (rows, Some(format!("cannot read {}: {err}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::backend::zephyr::ZephyrBackend;

    fn workspace(dir: &str) -> Workspace {
        Workspace {
            dir: PathBuf::from(dir),
            origin: crate::backend::zephyr::workspace::WorkspaceOrigin::UserConfig,
            zephyr_base: PathBuf::from(format!("{dir}/zephyr")),
            venv: None,
            west: "west".to_string(),
            sdk: None,
        }
    }

    fn zephyr_caps() -> crate::backend::Capabilities {
        ZephyrBackend.capabilities()
    }

    #[test]
    fn the_checklist_questions_come_before_the_operation_buttons() {
        let panel = WorkspacePanel::new(Resolution::Single(workspace("/opt/myzephyr")), "");
        assert_eq!(panel.dir(), Some(&PathBuf::from("/opt/myzephyr")));
        assert_eq!(
            panel.actions(&zephyr_caps()),
            vec![
                WorkspaceAction::Choose,
                WorkspaceAction::Projects,
                WorkspaceAction::Project,
                WorkspaceAction::Board,
                WorkspaceAction::Shield,
                WorkspaceAction::Update,
                WorkspaceAction::SdkList
            ]
        );
        for action in panel.actions(&zephyr_caps()) {
            assert!(
                panel.action_enabled(action),
                "a resolved pane enables every action"
            );
        }
    }

    #[test]
    fn an_unresolved_pane_keeps_the_buttons_visible_but_disabled() {
        let not_configured = WorkspacePanel::new(Resolution::NotConfigured, "");
        assert_eq!(
            not_configured.actions(&zephyr_caps()),
            vec![
                WorkspaceAction::Choose,
                WorkspaceAction::Projects,
                WorkspaceAction::Project,
                WorkspaceAction::Board,
                WorkspaceAction::Shield,
                WorkspaceAction::Update,
                WorkspaceAction::SdkList
            ]
        );
        assert!(not_configured.action_enabled(WorkspaceAction::Choose));
        assert!(not_configured.action_enabled(WorkspaceAction::Projects));
        assert!(not_configured.action_enabled(WorkspaceAction::Project));
        assert!(not_configured.action_enabled(WorkspaceAction::Board));
        assert!(not_configured.action_enabled(WorkspaceAction::Shield));
        assert!(
            !not_configured.action_enabled(WorkspaceAction::Update),
            "west update has nothing to run against yet"
        );
        assert!(!not_configured.action_enabled(WorkspaceAction::SdkList));

        let invalid = WorkspacePanel::new(Resolution::Invalid("no .west/ …".to_string()), "");
        assert!(invalid.dir().is_none());
        assert!(invalid.invalid.as_deref().unwrap().contains(".west"));
        assert!(!invalid.action_enabled(WorkspaceAction::Update));
    }

    #[test]
    fn a_fresh_resolution_always_wins() {
        let mut panel = WorkspacePanel::new(Resolution::NotConfigured, "");
        panel.apply_resolution(Resolution::Single(workspace("/opt/a")));
        assert_eq!(panel.dir(), Some(&PathBuf::from("/opt/a")));

        panel.apply_resolution(Resolution::Single(workspace("/opt/b")));
        assert_eq!(
            panel.dir(),
            Some(&PathBuf::from("/opt/b")),
            "no session pick: the config is the answer"
        );
    }

    #[test]
    fn workspace_commands_run_in_the_workspace_with_the_derived_environment() {
        let ws = workspace("/opt/myzephyr");
        let panel = WorkspacePanel::new(Resolution::Single(ws), "/usr/bin");

        let update = panel.update_command(&ZephyrBackend).unwrap();
        assert_eq!(update.cwd(), Some(&PathBuf::from("/opt/myzephyr")));
        assert_eq!(update.to_string(), "west update");
        assert_eq!(
            update.envs_slice(),
            [(
                "ZEPHYR_BASE".to_string(),
                "/opt/myzephyr/zephyr".to_string()
            )]
        );
        let sdk = panel.sdk_list_command(&ZephyrBackend).unwrap();
        assert!(sdk.to_string().ends_with("west sdk list"));
    }

    #[test]
    fn an_unresolved_pane_builds_no_commands() {
        let panel = WorkspacePanel::new(Resolution::NotConfigured, "");
        assert!(panel.update_command(&ZephyrBackend).is_none());
        assert!(panel.sdk_list_command(&ZephyrBackend).is_none());
    }

    #[test]
    fn dir_rows_offer_use_parent_then_subdirectories_sorted() {
        let tmp = std::env::temp_dir().join(format!("chiptui-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("beta")).unwrap();
        std::fs::create_dir_all(tmp.join("alpha")).unwrap();
        std::fs::write(tmp.join("file.txt"), b"").unwrap();

        let (rows, error) = dir_rows(&tmp);
        assert!(error.is_none());
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["use this directory", "..", "alpha", "beta"],
            "files never list; directories sort"
        );
        assert_eq!(rows[0].kind, DirRowKind::Use);
        assert_eq!(rows[1].kind, DirRowKind::Parent);
        assert_eq!(rows[2].path, tmp.join("alpha"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_unreadable_directory_still_navigates() {
        let (rows, error) = dir_rows(Path::new("/nonexistent-no-such-dir"));
        assert!(error.is_some(), "the read failure is reported");
        assert_eq!(rows.len(), 2, "the 'use' row and the parent remain");
        assert_eq!(rows[0].kind, DirRowKind::Use);
        assert_eq!(rows[1].kind, DirRowKind::Parent);
        assert_eq!(rows[1].path, Path::new("/"));
    }
}
