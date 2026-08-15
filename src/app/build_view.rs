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
    /// again.
    pub(super) fn run_build_action(&mut self, action: BuildAction) {
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
        }
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
