//! The workspace pane: the Zephyr backend's *environment* state machine,
//! the row-2 counterpart of [`crate::build::BuildPanel`].
//!
//! Where the build panel owns what happens *in the project* (build, flash,
//! board), this one owns what is *shared across projects*: which west
//! workspace the commands run against, which `west` executable (the
//! workspace's venv, when it has one), which SDK --- plus the two
//! workspace-scoped operations, `west update` and `west sdk list`
//! (`Capability::WorkspaceSync`).
//!
//! Like every panel here it is a pure state machine: resolution happens in
//! [`crate::backend::zephyr::workspace`], commands are built by the backend
//! ([`crate::backend::Backend::workspace_update_command`] and friends), and
//! running them is delegated to the build panel's single process slot ---
//! one backend, one running command, whatever pane started it.

use std::path::PathBuf;

use crate::backend::Capability;
use crate::backend::zephyr::workspace::{Resolution, Workspace, WorkspaceOrigin};
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
    /// Re-opens the workspace picker over the discovered candidates
    /// (session-only, like a board pick --- `SPEC.md` §10's rule that a
    /// pick must not touch configuration, applied to the environment).
    Choose,
}

pub struct WorkspacePanel {
    /// The current answer, once resolution or a pick produced one. `None`
    /// while unresolved (missing, invalid, or waiting for a pick).
    pub resolved: Option<Workspace>,
    /// Why an explicitly configured workspace failed validation, when one
    /// did (`Resolution::Invalid`).
    pub invalid: Option<String>,
    /// Every workspace discovery found, best first --- the picker's rows,
    /// kept after resolution so `Choose` can reopen it.
    pub candidates: Vec<Workspace>,
    pub cursor: usize,
    /// The inherited `PATH` at startup, baked into every derived
    /// [`crate::backend::zephyr::workspace::WestEnv`] (the venv's `bin` is
    /// prepended to it, never replacing it).
    path_env: String,
}

impl WorkspacePanel {
    /// Builds the pane from a resolution, keeping every candidate for the
    /// picker. `path_env` is the process's own `PATH` (an empty string is
    /// fine --- only venv workspaces prepend anything to it).
    pub fn new(resolution: Resolution, path_env: impl Into<String>) -> Self {
        let mut panel = Self {
            resolved: None,
            invalid: None,
            candidates: Vec::new(),
            cursor: 0,
            path_env: path_env.into(),
        };
        panel.apply_resolution(resolution);
        panel
    }

    /// Applies a fresh resolution, preserving a session pick: a pick
    /// outranks re-discovery exactly the way a board pick outranks a cache
    /// re-read.
    pub fn apply_resolution(&mut self, resolution: Resolution) {
        match resolution {
            Resolution::Single(workspace) => {
                self.candidates = vec![workspace.clone()];
                self.keep_or_replace(workspace);
            }
            Resolution::Ambiguous(candidates) => {
                self.candidates = candidates;
                // Unresolved *and* multiple answers: the pane prompts, the
                // picker decides. A previous pick survives even this.
                if self.resolved.is_none() {
                    self.cursor = 0;
                }
            }
            Resolution::Invalid(message) => {
                self.invalid = Some(message);
                self.candidates.clear();
            }
            Resolution::Missing => {
                self.candidates.clear();
            }
        }
    }

    fn keep_or_replace(&mut self, workspace: Workspace) {
        if self
            .resolved
            .as_ref()
            .is_none_or(|current| current.origin != WorkspaceOrigin::Picked)
        {
            self.resolved = Some(workspace);
        }
    }

    /// Applies a picker choice: session-only, never written to any config
    /// (the way home is recorded is the config file's business, and the
    /// user's).
    pub fn set_picked(&mut self, index: usize) {
        if let Some(workspace) = self.candidates.get(index).cloned() {
            let mut workspace = workspace;
            workspace.origin = WorkspaceOrigin::Picked;
            self.resolved = Some(workspace);
            self.cursor = 0;
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

    /// Rows the action list shows: the workspace operations once a
    /// workspace is resolved, and the chooser whenever there is a choice to
    /// make or remake. An unresolved pane with nothing to choose shows no
    /// rows --- the status explains what to configure instead.
    pub fn actions(&self, caps: &crate::backend::Capabilities) -> Vec<WorkspaceAction> {
        if !caps.contains(Capability::WorkspaceSync) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        if self.resolved.is_some() {
            actions.push(WorkspaceAction::Update);
            actions.push(WorkspaceAction::SdkList);
        }
        if self.candidates.len() > 1 {
            actions.push(WorkspaceAction::Choose);
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

    /// The workspace's location for status display, when one is resolved.
    pub fn dir(&self) -> Option<&PathBuf> {
        self.resolved.as_ref().map(|workspace| &workspace.dir)
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
            origin: WorkspaceOrigin::HomeDefault,
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
    fn a_single_resolution_resolves_and_lists_workspace_operations() {
        let ws = workspace("/home/dev/zephyrproject");
        let panel = WorkspacePanel::new(Resolution::Single(ws.clone()), "");
        assert_eq!(panel.dir(), Some(&PathBuf::from("/home/dev/zephyrproject")));
        assert_eq!(
            panel.actions(&zephyr_caps()),
            vec![WorkspaceAction::Update, WorkspaceAction::SdkList]
        );
    }

    #[test]
    fn ambiguity_stays_unresolved_until_picked() {
        let candidates = vec![workspace("/a"), workspace("/b")];
        let mut panel = WorkspacePanel::new(Resolution::Ambiguous(candidates.clone()), "");
        assert!(panel.dir().is_none());
        assert_eq!(
            panel.actions(&zephyr_caps()),
            vec![WorkspaceAction::Choose],
            "choosing is the only action that makes sense"
        );

        panel.set_picked(1);
        assert_eq!(panel.dir(), Some(&PathBuf::from("/b")));
        assert_eq!(
            panel.resolved.as_ref().unwrap().origin,
            WorkspaceOrigin::Picked
        );
    }

    #[test]
    fn a_pick_survives_rediscovery() {
        let mut panel = WorkspacePanel::new(Resolution::Ambiguous(vec![workspace("/a")]), "");
        panel.set_picked(0);

        panel.apply_resolution(Resolution::Single(workspace("/a")));
        assert_eq!(
            panel.resolved.as_ref().unwrap().origin,
            WorkspaceOrigin::Picked,
            "a session pick outranks re-discovery"
        );
    }

    #[test]
    fn missing_and_invalid_panes_explain_instead_of_offering_actions() {
        let missing = WorkspacePanel::new(Resolution::Missing, "");
        assert!(missing.actions(&zephyr_caps()).is_empty());

        let invalid = WorkspacePanel::new(
            Resolution::Invalid("the workspace configured … has no .west/ directory".to_string()),
            "",
        );
        assert!(invalid.dir().is_none());
        assert!(invalid.invalid.as_deref().unwrap().contains(".west"));
        assert!(invalid.actions(&zephyr_caps()).is_empty());
    }

    #[test]
    fn workspace_commands_run_in_the_workspace_with_the_derived_environment() {
        let ws = workspace("/home/dev/zephyrproject");
        let panel = WorkspacePanel::new(Resolution::Single(ws), "/usr/bin");

        let update = panel.update_command(&ZephyrBackend).unwrap();
        assert_eq!(
            update.cwd(),
            Some(&PathBuf::from("/home/dev/zephyrproject"))
        );
        assert_eq!(
            update.to_string(),
            "ZEPHYR_BASE=/home/dev/zephyrproject/zephyr west update"
        );
        let sdk = panel.sdk_list_command(&ZephyrBackend).unwrap();
        assert!(sdk.to_string().ends_with("west sdk list"));
    }

    #[test]
    fn an_unresolved_pane_builds_no_commands() {
        let panel = WorkspacePanel::new(Resolution::Missing, "");
        assert!(panel.update_command(&ZephyrBackend).is_none());
        assert!(panel.sdk_list_command(&ZephyrBackend).is_none());
    }
}
