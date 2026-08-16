//! The workspace pane: the Zephyr backend's *environment* state machine,
//! the row-2 counterpart of [`crate::build::BuildPanel`].
//!
//! Where the build panel owns what happens *in the project* (build, flash,
//! board, plus the workspace-scoped `west update`/`west sdk list` it runs
//! through --- see [`crate::build::BuildAction`]), this one owns what is
//! *shared across projects*: which Zephyr installation the commands run
//! against, which `west` executable (the workspace's venv, when it has
//! one), which SDK. Below that checklist it also owns the project's own
//! files --- view, edit, delete, create --- so the one pane covers every
//! open question plus the sources those answers unlock.
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
use crate::files::LocalEntry;
use crate::process::Command;

/// One row of the workspace pane's checklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAction {
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
    /// Whether the cursor is currently inside the embedded file list rather
    /// than the checklist above it: while `false`, `cursor` indexes
    /// [`Self::actions`]; while `true`, `files_cursor` indexes
    /// [`Self::visible_files`]. One pane, one cursor, two regions --- the
    /// same shape [`crate::browser::Browser`] already uses for its own two
    /// navigable sides.
    pub in_files: bool,
    /// The project root the file list is scoped to. Navigation never rises
    /// above it (see [`Self::ascend_files`]) --- browsing anywhere else is
    /// the `Choose`/`Projects` pickers' job, not this section's. Re-rooted
    /// alongside [`crate::build::BuildPanel::root`] by [`Self::set_files_root`].
    pub files_root: PathBuf,
    /// The directory currently listed; starts at `files_root`.
    pub files_path: PathBuf,
    pub files_entries: Vec<LocalEntry>,
    pub files_error: Option<String>,
    pub files_cursor: usize,
}

impl WorkspacePanel {
    /// Builds the pane from a resolution. `path_env` is the process's own
    /// `PATH` (an empty string is fine --- only venv workspaces prepend
    /// anything to it). The file list starts empty --- [`Self::set_files_root`]
    /// is called once the project root is known, before the first frame ever
    /// draws it.
    pub fn new(resolution: Resolution, path_env: impl Into<String>) -> Self {
        let mut panel = Self {
            resolved: None,
            invalid: None,
            projects: None,
            projects_invalid: None,
            cursor: 0,
            path_env: path_env.into(),
            in_files: false,
            files_root: PathBuf::new(),
            files_path: PathBuf::new(),
            files_entries: Vec::new(),
            files_error: None,
            files_cursor: 0,
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

    /// Rows the checklist shows: the installation, the projects folder, the
    /// project, the board and its optional shield --- the prerequisites in
    /// the order they are answered. Every row here is always answerable
    /// (`Enter` on any of them opens its picker, which is also how to
    /// *change* an answer later), so there is no analog of a disabled row to
    /// account for.
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
        actions
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

    /// Re-roots the embedded file list to `dir` (the build panel's project
    /// root): resets the browsed path back to it, clears the file cursor and
    /// returns focus to the checklist --- a re-root is a fresh start, the
    /// same rule [`crate::build::BuildPanel::set_project`] follows for its
    /// own cursor.
    pub fn set_files_root(&mut self, dir: impl Into<PathBuf>) {
        let dir = dir.into();
        self.files_root = dir.clone();
        self.files_path = dir;
        self.files_cursor = 0;
        self.in_files = false;
        self.reload_files();
    }

    /// Re-reads the currently browsed directory.
    pub fn reload_files(&mut self) {
        match crate::files::read_dir(&self.files_path) {
            Ok(entries) => {
                self.files_entries = entries;
                self.files_error = None;
            }
            Err(source) => {
                self.files_entries.clear();
                self.files_error = Some(format!(
                    "cannot read {}: {source}",
                    self.files_path.display()
                ));
            }
        }
        self.files_cursor = self
            .files_cursor
            .min(self.files_entries.len().saturating_sub(1));
    }

    /// Entries the file list shows.
    pub fn visible_files(&self) -> &[LocalEntry] {
        &self.files_entries
    }

    pub fn files_selected(&self) -> Option<&LocalEntry> {
        self.files_entries.get(self.files_cursor)
    }

    /// Descends into the directory under the cursor; a no-op on a file
    /// (mirrors [`crate::browser::Browser::enter`]'s local-pane arm).
    pub fn enter_files(&mut self) {
        let Some(entry) = self.files_selected() else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        self.files_path = self.files_path.join(&entry.name);
        self.files_cursor = 0;
        self.reload_files();
    }

    /// Steps up to the parent, floored at `files_root`: this section manages
    /// the project's own files, not the filesystem at large. Leaves the
    /// directory just left selected in its parent, the same "yazi" behavior
    /// [`crate::browser::Browser::ascend`] uses.
    pub fn ascend_files(&mut self) {
        if self.files_path == self.files_root {
            return;
        }
        let Some(parent) = self.files_path.parent().map(Path::to_path_buf) else {
            return;
        };
        let left = self
            .files_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        self.files_path = parent;
        self.reload_files();
        if let Some(left) = left
            && let Some(index) = self
                .files_entries
                .iter()
                .position(|entry| entry.name == left)
        {
            self.files_cursor = index;
        }
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
    fn the_checklist_is_the_full_action_list() {
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
            ],
            "west update/sdk list now live in the build panel's action list"
        );
    }

    #[test]
    fn an_unresolved_pane_still_offers_the_full_checklist() {
        let not_configured = WorkspacePanel::new(Resolution::NotConfigured, "");
        assert_eq!(
            not_configured.actions(&zephyr_caps()),
            vec![
                WorkspaceAction::Choose,
                WorkspaceAction::Projects,
                WorkspaceAction::Project,
                WorkspaceAction::Board,
                WorkspaceAction::Shield,
            ]
        );

        let invalid = WorkspacePanel::new(Resolution::Invalid("no .west/ …".to_string()), "");
        assert!(invalid.dir().is_none());
        assert!(invalid.invalid.as_deref().unwrap().contains(".west"));
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

    fn files_fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-workspace-files-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("CMakeLists.txt"),
            "find_package(Zephyr REQUIRED)\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn set_files_root_lists_the_new_root_and_resets_navigation() {
        let dir = files_fixture("reroot");
        let mut panel = WorkspacePanel::new(Resolution::NotConfigured, "");
        panel.in_files = true;
        panel.files_cursor = 3;

        panel.set_files_root(&dir);

        assert_eq!(panel.files_root, dir);
        assert_eq!(panel.files_path, dir);
        assert!(!panel.in_files, "a re-root returns to the checklist");
        assert_eq!(panel.files_cursor, 0);
        let names: Vec<&str> = panel
            .visible_files()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["src", "CMakeLists.txt"], "dirs sort first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_and_ascend_files_navigate_without_rising_above_the_root() {
        let dir = files_fixture("nav");
        let mut panel = WorkspacePanel::new(Resolution::NotConfigured, "");
        panel.set_files_root(&dir);

        // "src" sorts first (directories lead).
        assert_eq!(panel.files_selected().unwrap().name, "src");
        panel.enter_files();
        assert_eq!(panel.files_path, dir.join("src"));

        // A no-op file selection descend does nothing; ascend returns to the
        // root with "src" left selected (the directory just exited).
        panel.ascend_files();
        assert_eq!(panel.files_path, dir);
        assert_eq!(panel.files_selected().unwrap().name, "src");

        // Floored: ascending again from the root is a no-op.
        panel.ascend_files();
        assert_eq!(panel.files_path, dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_files_reports_an_unreadable_directory() {
        let dir = files_fixture("unreadable");
        let mut panel = WorkspacePanel::new(Resolution::NotConfigured, "");
        panel.set_files_root(dir.join("missing"));
        assert!(panel.files_error.is_some());
        assert!(panel.visible_files().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
