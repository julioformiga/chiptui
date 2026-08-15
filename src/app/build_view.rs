//! Build-panel driving: the action list's key handling, spawning commands,
//! and the confirm gate in front of [`crate::backend::Capability::Clean`]
//! (`SPEC.md` §15 --- destructive actions ask first, showing the literal
//! command). Split out of `app.rs` alongside the other one-subsystem files.

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

    /// Applies the board chosen in the picker: session-only, and the panel
    /// header says so (`SPEC.md` §10 --- a pick must not touch project
    /// configuration).
    pub(super) fn apply_board_picker(&mut self, filter: &str, selected: usize) {
        let Some(panel) = &mut self.build else {
            return;
        };
        let filtered = panel.filtered_boards(filter);
        let Some(name) = filtered.get(selected).map(|board| board.name.clone()) else {
            return;
        };
        panel.set_picked(name.clone());
        self.logs.info(format!(
            "board set to {name} for this session (nothing written)"
        ));
    }

    /// Runs a panel action: destructive ones (`Clean`, `Flash` --- both
    /// destructive capabilities, `SPEC.md` §15) route through a confirm
    /// overlay quoting the literal command; `menuconfig` hands the terminal
    /// to the child; the rest act immediately. This is also the confirm
    /// overlay's accept path, which is why it must not itself confirm
    /// again. Every action that runs a *project* command passes through
    /// [`Self::require_buildable_project`] first --- no command runs in a
    /// directory that is not a buildable application.
    pub(super) fn run_build_action(&mut self, action: BuildAction) {
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
            BuildAction::Board => self.open_board_picker(),
            BuildAction::Menuconfig => self.start_menuconfig(),
            BuildAction::BuildDir => self.open_build_dir_picker(),
            BuildAction::Project => self.open_project_flow(),
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
        let Some(panel) = &mut self.build else {
            return;
        };
        panel.set_project(row.path.clone());
        self.logs.info(format!(
            "project set to {} for this session (nothing written)",
            row.path.display()
        ));
        self.overlay = None;
    }

    /// Starts `kind`'s command and moves the user to where its output
    /// streams. A failure to even compose or start the command is a log
    /// notice instead: the panel stays usable.
    pub(super) fn start_build(&mut self, kind: BuildKind) {
        let updates_board = matches!(kind, BuildKind::Build | BuildKind::Rebuild);
        self.start_build_command(kind.label(), updates_board, |panel, backend| {
            panel.command(kind, backend)
        });
    }

    /// Starts the flash command, same hand-off as the build kinds. Reached
    /// only through the confirm overlay (flash is destructive).
    pub(super) fn start_flash(&mut self) {
        self.start_build_command("Flash", false, |panel, backend| {
            panel.flash_command(backend)
        });
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

    /// Opens the build-directory picker over the project's configured
    /// directories; a typed name that matches nothing starts a new one.
    pub(super) fn open_build_dir_picker(&mut self) {
        self.overlay = Some(Overlay::BuildDirPicker {
            input: String::new(),
            selected: 0,
        });
    }

    /// Applies the build-directory choice (picked row or typed name):
    /// session-only --- the directory is an argument on the next command,
    /// never a persisted setting (`SPEC.md` §10's no-silent-writes rule).
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
    /// compose through the panel, run, and move the user to the Monitor
    /// tab where the output streams.
    fn start_build_command(
        &mut self,
        label: &'static str,
        updates_board: bool,
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
        if !panel.start(label, updates_board, command, &mut self.processes) {
            return;
        }
        self.logs.info(format!("running {full_label}"));
        // Same hand-off as the flash dialog: the command's home while it
        // runs is the Monitor tab (`SPEC.md` §11).
        self.view = super::View::Dashboard;
        self.focus = Focus::Logs;
        self.log_tab = LogTab::Monitor;
        self.monitor_source = MonitorSource::Build;
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
