//! Build-panel driving: the action list's key handling, spawning commands,
//! and the confirm gate in front of [`crate::backend::Capability::Clean`]
//! (`SPEC.md` §15 --- destructive actions ask first, showing the literal
//! command). Split out of `app.rs` alongside the other one-subsystem files.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::backend::BuildKind;
use crate::build::BuildAction;

use super::{App, Focus, LogTab, MonitorSource, Overlay};

impl App {
    /// Handles a key while [`Focus::Build`] holds focus. The list is
    /// navigated like every other list here (`j`/`k`, arrows, page, home/
    /// end); `Enter` runs the action under the cursor --- `Stop` while a
    /// command is running, otherwise the build lifecycle entry.
    pub(super) fn on_build_key(&mut self, key: KeyEvent) {
        // Two phases: cursor movement borrows only the panel, `Enter` needs
        // the whole app (starting a command flips focus and the monitor
        // source), so the decision is computed first and acted on after the
        // panel borrow ends.
        let caps = self.manager.capabilities();
        let mut action = None;
        if let Some(panel) = self.build.as_mut() {
            let len = panel.actions(&caps).len();
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
            self.run_build_action(action);
        }
    }

    /// Opens the board picker, kicking off the background `west boards`
    /// fetch on first open (the list is slow to produce and useless until
    /// asked for).
    pub(super) fn open_board_picker(&mut self) {
        self.overlay = Some(Overlay::BoardPicker {
            input: String::new(),
            selected: 0,
        });
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(panel) = &mut self.build else {
            return;
        };
        if let Some(command) = panel.boards_command(backend) {
            let label = command.to_string();
            panel.start_boards_fetch(command, &mut self.processes);
            self.logs.info(format!("fetching the board list ({label})"));
        }
    }

    /// Applies the board chosen in the picker and persists it in the
    /// project's registry entry (`SPEC.md` §13): the answer reloads on
    /// every later open, outranking the build cache. Nothing lands in the
    /// project directory --- the registry is the one place a session
    /// answer outlives the session.
    pub(super) fn apply_board_picker(&mut self, filter: &str, selected: usize) {
        let Some(panel) = &mut self.build else {
            return;
        };
        let filtered = panel.filtered_boards(filter);
        let Some(name) = filtered.get(selected).map(|board| board.name.clone()) else {
            return;
        };
        panel.set_picked(name.clone());
        self.logs.info(format!("board set to {name}"));
        self.persist_board_shield();
    }

    /// Writes the panel's current board and shield answers into the
    /// registry entry for the project the panel is rooted at (creating the
    /// entry when the project is not recorded yet --- opening it would do
    /// the same). Everything else already recorded --- backend, name,
    /// last-opened stamp --- survives untouched.
    fn persist_board_shield(&mut self) {
        let Some(panel) = &self.build else {
            return;
        };
        let Some(kind) = self.manager.selected_kind() else {
            return;
        };
        let root = panel.root.clone();
        let mut entry = match self.manager.known_projects().entry_for(&root) {
            Some(known) => known.clone(),
            None => crate::settings::ProjectEntry::new(&root, kind),
        };
        entry.board = panel.board_name().map(str::to_string);
        entry.shield = panel.shield_name().map(str::to_string);
        let config = self.user_config_path();
        match crate::settings::record_project(&config, entry) {
            Ok(()) => self
                .logs
                .info(format!("board/shield answer saved to {}", config.display())),
            Err(err) => self.logs.warn(format!(
                "could not save the board/shield answer in {}: {err}",
                config.display()
            )),
        }
        self.manager
            .set_known_projects(crate::settings::ProjectRegistry::load(
                &self.config_dir,
                &self.home_dir,
            ));
    }

    /// Opens the shield picker, kicking off the background `west shields`
    /// fetch on first open, like the board picker does for `west boards`.
    pub(super) fn open_shield_picker(&mut self) {
        self.overlay = Some(Overlay::ShieldPicker {
            input: String::new(),
            selected: 0,
        });
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(panel) = &mut self.build else {
            return;
        };
        if let Some(command) = panel.shields_command(backend) {
            let label = command.to_string();
            panel.start_shields_fetch(command, &mut self.processes);
            self.logs
                .info(format!("fetching the shield list ({label})"));
        }
    }

    /// Applies the shield picker's answer and persists it beside the board:
    /// row 0 --- the `(none)` row --- clears it (the shield is optional, so
    /// no pick is a valid answer, unlike the board).
    pub(super) fn apply_shield_picker(&mut self, filter: &str, selected: usize) {
        let Some(panel) = &mut self.build else {
            return;
        };
        if selected == 0 {
            panel.set_shield(None);
            self.logs.info("shield cleared");
            self.persist_board_shield();
            return;
        }
        let filtered = panel.filtered_shields(filter);
        let Some(name) = filtered.get(selected - 1).map(|shield| shield.name.clone()) else {
            return;
        };
        panel.set_shield(Some(name.clone()));
        self.logs.info(format!("shield set to {name}"));
        self.persist_board_shield();
    }

    /// Runs a panel action: destructive ones (`Clean`, `Flash`, and
    /// `UpdateZephyr` --- it rewrites the shared workspace, `SPEC.md` §15)
    /// route through a confirm overlay quoting the literal command;
    /// `menuconfig` hands the terminal to the child; `SdkList` (read-only)
    /// and the rest act immediately. Disabled rows (the lifecycle before the
    /// checklist is complete, or `UpdateZephyr`/`SdkList` before a workspace
    /// is resolved --- [`Self::build_action_enabled`]) do nothing: the
    /// dimmed row is the explanation. This is also the confirm overlay's
    /// accept path, which is why it must not itself confirm again. Every
    /// action that runs a *project* command passes through
    /// [`Self::require_buildable_project`] first --- no command runs in a
    /// directory that is not a buildable application; `UpdateZephyr`/
    /// `SdkList` skip that gate, since they run in the workspace, not the
    /// project.
    pub(super) fn run_build_action(&mut self, action: BuildAction) {
        if !self.build_action_enabled(action) {
            return;
        }
        if matches!(
            action,
            BuildAction::Build(_) | BuildAction::Flash | BuildAction::Menuconfig
        ) && !self.require_buildable_project(&action)
        {
            return;
        }
        match action {
            BuildAction::Stop => self.stop_build(),
            BuildAction::Build(kind) => {
                if kind == BuildKind::Clean {
                    self.overlay = Some(Overlay::ConfirmBuild {
                        action,
                        confirm: false,
                    });
                } else {
                    self.start_build(kind);
                }
            }
            BuildAction::Flash => {
                self.overlay = Some(Overlay::ConfirmBuild {
                    action,
                    confirm: false,
                });
            }
            BuildAction::Menuconfig => self.start_menuconfig(),
            BuildAction::UpdateZephyr => {
                self.overlay = Some(Overlay::ConfirmBuild {
                    action,
                    confirm: false,
                });
            }
            BuildAction::SdkList => {
                self.start_workspace_command("SDK list", BuildAction::SdkList, |panel, backend| {
                    panel.sdk_list_command(backend)
                });
            }
        }
    }

    /// The project gate (`Capability::ProjectSelect`): every command that
    /// runs *in the project* (build, clean, rebuild, flash, menuconfig)
    /// needs a working directory with build elements. A root that has them
    /// --- the directory ChipTUI was started in, when that is a project ---
    /// passes without ceremony; one that does not is refused, never
    /// silently built around: the refusal explains itself and opens the
    /// flow that answers it (projects folder, then the project itself).
    fn require_buildable_project(&mut self, action: &BuildAction) -> bool {
        let Some(panel) = &self.build else {
            return true;
        };
        if !self
            .manager
            .capabilities()
            .contains(crate::backend::Capability::ProjectSelect)
            || crate::backend::zephyr::projects::is_buildable(&panel.root)
        {
            return true;
        }
        let what = match action {
            BuildAction::Build(kind) => kind.label(),
            BuildAction::Flash => "flash",
            BuildAction::Menuconfig => "menuconfig",
            _ => "this command",
        };
        self.logs.warn(format!(
            "{what}: {} is not a Zephyr application (no CMakeLists.txt) — pick a project first",
            panel.root.display()
        ));
        self.open_project_flow();
        false
    }

    /// Opens whichever picker the project question needs next: the projects
    /// folder when none is configured, the project list when one is.
    pub(super) fn open_project_flow(&mut self) {
        if self
            .workspace
            .as_ref()
            .is_some_and(|panel| panel.projects.is_some())
        {
            self.open_project_picker();
        } else {
            self.logs
                .warn("no projects folder configured — where do your Zephyr applications live?");
            self.open_projects_dir_picker();
        }
    }

    /// Opens the project picker over the configured projects folder. The
    /// rows (and each one's build-element mark) are read at draw time like
    /// every other overlay's derived state.
    pub(super) fn open_project_picker(&mut self) {
        self.overlay = Some(Overlay::ProjectPicker {
            selected: 0,
            error: None,
        });
    }

    /// Applies the project chosen in the picker: session-only, re-rooting
    /// every build command (`west` runs there; nothing is written --- the
    /// folder is the persisted half of the answer, the project is not).
    /// Accepting a directory without build elements keeps the picker open
    /// with the reason: the verification is the point.
    pub(super) fn apply_project_picker(&mut self, selected: usize) {
        let Some(dir) = self
            .workspace
            .as_ref()
            .and_then(|panel| panel.projects.clone())
        else {
            self.open_project_flow();
            return;
        };
        let (rows, read_error) = crate::backend::zephyr::projects::project_rows(&dir);
        let Some(row) = rows.get(selected) else {
            let reason = read_error.unwrap_or_else(|| "nothing to pick".to_string());
            self.overlay = Some(Overlay::ProjectPicker {
                selected,
                error: Some(reason),
            });
            return;
        };
        if !row.buildable {
            self.overlay = Some(Overlay::ProjectPicker {
                selected,
                error: Some(format!(
                    "{} has no CMakeLists.txt — west build cannot run there",
                    row.name
                )),
            });
            return;
        }
        if self.build.is_none() {
            return;
        }
        self.set_project_root(row.path.clone());
        self.logs.info(format!(
            "project set to {} for this session (nothing written)",
            row.path.display()
        ));
        self.overlay = None;
    }

    /// Re-roots the build lifecycle and the workspace pane's embedded file
    /// list to `dir` together: picking a project answers both "what does
    /// `west` build" and "what files does the Workspace pane show" at once,
    /// and letting them diverge would leave the file list showing a
    /// directory the build panel no longer targets. The picked project's
    /// own registry answers (board, shield) are re-applied after the
    /// re-root: a saved answer belongs to the project, not the session.
    pub(super) fn set_project_root(&mut self, dir: PathBuf) {
        if let Some(panel) = &mut self.build {
            panel.set_project(dir.clone());
        }
        if let Some(entry) = self.manager.known_projects().entry_for(&dir) {
            let board = entry.board.clone();
            let shield = entry.shield.clone();
            if let Some(panel) = &mut self.build {
                if let Some(board) = board {
                    panel.set_config_board(board);
                }
                if let Some(shield) = shield {
                    panel.set_shield(Some(shield));
                }
            }
        }
        if let Some(workspace) = &mut self.workspace {
            workspace.set_files_root(dir);
        }
    }

    /// Starts `kind`'s command and shows its output streaming in the
    /// Monitor tab --- without pulling focus off the panel: the lifecycle's
    /// next step is right here (Stop while it runs, then Flash on success).
    /// A failure to even compose or start the command is a log notice
    /// instead: the panel stays usable.
    pub(super) fn start_build(&mut self, kind: BuildKind) {
        let updates_board = matches!(kind, BuildKind::Build | BuildKind::Rebuild);
        let caps = self.manager.capabilities();
        self.start_build_command(
            kind.label(),
            updates_board,
            BuildAction::Build(kind),
            Focus::Build,
            |panel, backend| panel.command(kind, backend),
        );
        // Clean parks the cursor on Build: the build is the step a clean
        // exists to clear the way for (a build/rebuild already sits on
        // Stop, the row `start` lands on).
        if kind == BuildKind::Clean
            && let Some(panel) = self.build.as_mut()
        {
            panel.focus_action(&caps, BuildAction::Build(BuildKind::Build));
        }
    }

    /// Starts the flash command, same hand-off as the build kinds except
    /// that focus follows the output (the device is the thing changing).
    /// Reached only through the confirm overlay (flash is destructive).
    pub(super) fn start_flash(&mut self) {
        self.start_build_command(
            "Flash",
            false,
            BuildAction::Flash,
            Focus::Logs,
            |panel, backend| panel.flash_command(backend),
        );
    }

    /// Hands the terminal to `west build -t menuconfig` (`SPEC.md` §11's
    /// terminal-native rule: the Kconfig editor *is* a full-screen TUI, and
    /// nesting it in a pane would break both). The command is parked for
    /// the event loop, which owns the terminal guard needed to suspend the
    /// alternate screen --- the same hand-off as `$EDITOR`.
    pub(super) fn start_menuconfig(&mut self) {
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(panel) = &self.build else {
            return;
        };
        let Some(command) = panel.menuconfig_command(backend) else {
            self.logs
                .warn("menuconfig: this backend offers no such action");
            return;
        };
        self.logs
            .info(format!("running {command} (the TUI is suspended)"));
        self.pending_command = Some(command);
    }

    /// Applies the build-directory choice (picked row or typed name):
    /// session-only --- the directory is an argument on the next command,
    /// never a persisted setting (`SPEC.md` §10's no-silent-writes rule).
    /// The panel's list no longer offers the picker (the lifecycle targets
    /// the conventional `build`), but the overlay and this path remain.
    pub(super) fn apply_build_dir_picker(&mut self, filter: &str, selected: usize) {
        let Some(panel) = &mut self.build else {
            return;
        };
        let dirs = panel.filtered_build_dirs(filter);
        let Some(dir) = dirs.get(selected).cloned() else {
            return;
        };
        panel.set_build_dir(dir.clone());
        self.logs
            .info(format!("build directory set to {dir} for this session"));
    }

    /// The shared body of [`Self::start_build`]/[`Self::start_flash`]:
    /// compose through the panel, run, and point the Monitor tab at the
    /// output. `focus` is where the user sits while it runs --- the panel
    /// for the build lifecycle, the Monitor tab for a flash --- and
    /// `action` is the row that was started, so the panel's cursor can
    /// return there (or its lifecycle successor) when it finishes.
    fn start_build_command(
        &mut self,
        label: &'static str,
        updates_board: bool,
        action: BuildAction,
        focus: Focus,
        command: impl FnOnce(
            &mut crate::build::BuildPanel,
            &dyn crate::backend::Backend,
        ) -> Option<crate::process::Command>,
    ) {
        let Some(backend) = self.manager.backend() else {
            return;
        };
        let Some(panel) = &mut self.build else {
            return;
        };
        if panel.is_busy() {
            self.logs.warn("a build command is already running");
            return;
        }
        let Some(command) = command(panel, backend) else {
            self.logs
                .warn(format!("{label}: this backend offers no such action"));
            return;
        };
        let full_label = command.to_string();
        let caps = self.manager.capabilities();
        if !panel.start(
            label,
            updates_board,
            action,
            command,
            &mut self.processes,
            &caps,
        ) {
            return;
        }
        self.logs.info(format!("running {full_label}"));
        // The command's home while it runs is the Monitor tab (`SPEC.md`
        // §11) --- showing it, without moving the user off the panel.
        self.view = super::View::Dashboard;
        self.focus = focus;
        self.log_tab = LogTab::Monitor;
        self.set_monitor_source(MonitorSource::Build);
    }

    /// Cancels the running build command at the user's request.
    pub(super) fn stop_build(&mut self) {
        let Some(panel) = &mut self.build else {
            return;
        };
        if panel.stop(&mut self.processes) {
            self.logs.warn("stopping the build command");
        }
    }
}
