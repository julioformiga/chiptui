//! Workspace-pane driving: the action list's key handling, the workspace
//! picker, and the confirm gate in front of `west update` (which rewrites
//! the shared workspace --- `SPEC.md` §15's never-hide-destruction, applied
//! outside the project). Split out of `app.rs` alongside the other
//! one-subsystem files.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::workspace::WorkspaceAction;

use super::{App, Focus, LogTab, MonitorSource, Overlay};

impl App {
    /// Handles a key while [`Focus::Workspace`] holds focus: the same list
    /// navigation grammar as the build panel (`j`/`k`, arrows, page,
    /// home/end), with `Enter` running the action under the cursor.
    pub(super) fn on_workspace_key(&mut self, key: KeyEvent) {
        let caps = self.manager.capabilities();
        let mut action = None;
        if let Some(panel) = self.workspace.as_mut() {
            let len = panel.actions(&caps).len().max(1);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    panel.cursor = panel.cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    panel.cursor = (panel.cursor + 1).min(len - 1);
                }
                KeyCode::PageUp => panel.cursor = panel.cursor.saturating_sub(5),
                KeyCode::PageDown => panel.cursor = (panel.cursor + 5).min(len - 1),
                KeyCode::Home => panel.cursor = 0,
                KeyCode::End => panel.cursor = len - 1,
                KeyCode::Enter => action = panel.action_at(&caps, panel.cursor),
                _ => {}
            }
        }
        if let Some(action) = action {
            self.run_workspace_action(action);
        }
    }

    /// Runs a workspace action: `west update` confirms first (it rewrites
    /// the workspace every project in it shares), `west sdk list` and the
    /// picker act immediately.
    pub(super) fn run_workspace_action(&mut self, action: WorkspaceAction) {
        match action {
            WorkspaceAction::Update => {
                self.overlay = Some(Overlay::ConfirmWorkspace {
                    action,
                    confirm: false,
                });
            }
            WorkspaceAction::SdkList => self
                .start_workspace_command("SDK list", |panel, backend| {
                    panel.sdk_list_command(backend)
                }),
            WorkspaceAction::Choose => self.open_workspace_picker(),
        }
    }

    /// Opens the workspace picker over the discovered candidates. `Esc`
    /// leaves everything as it was --- an unresolved pane stays unresolved,
    /// a resolved one keeps its answer.
    pub(super) fn open_workspace_picker(&mut self) {
        if self
            .workspace
            .as_ref()
            .is_some_and(|panel| panel.candidates.len() > 1)
        {
            self.overlay = Some(Overlay::WorkspacePicker { selected: 0 });
        }
    }

    /// Applies the picker's choice: session-only, never written anywhere
    /// (the way a workspace is recorded permanently is the config file, and
    /// only the user edits that).
    pub(super) fn apply_workspace_picker(&mut self, selected: usize) {
        if let Some(panel) = &mut self.workspace {
            panel.set_picked(selected);
            if let Some(workspace) = panel.resolved.clone() {
                self.logs.info(format!(
                    "workspace set to {} for this session (nothing written)",
                    workspace.dir.display()
                ));
            }
        }
        // The build panel's commands must follow the new answer.
        self.apply_west_env();
    }

    /// Pushes the resolved workspace's west invocation (executable and
    /// environment) into the build panel, whose commands are where it
    /// matters.
    fn apply_west_env(&mut self) {
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
    /// moves the user to where its output streams.
    fn start_workspace_command(
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
        if !panel.start(label, false, command, &mut self.processes) {
            return;
        }
        self.view = super::View::Dashboard;
        self.focus = Focus::Logs;
        self.log_tab = LogTab::Monitor;
        self.monitor_source = MonitorSource::Build;
    }

    /// The confirm overlay's accept path for `west update`.
    pub(super) fn start_workspace_update(&mut self) {
        self.start_workspace_command("West update", |panel, backend| {
            panel.update_command(backend)
        });
    }
}
