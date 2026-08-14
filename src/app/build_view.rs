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
        let board_select = self
            .manager
            .capabilities()
            .contains(crate::backend::Capability::BoardSelect);
        let mut action = None;
        if let Some(panel) = self.build.as_mut() {
            let len = panel.actions(board_select).len();
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
                KeyCode::Enter => action = panel.action_at(board_select, panel.cursor),
                _ => {}
            }
        }
        match action {
            Some(BuildAction::Stop) => self.stop_build(),
            Some(BuildAction::Build(kind)) => self.queue_build_action(kind),
            Some(BuildAction::Board) => self.open_board_picker(),
            None => {}
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

    /// Entry point for a chosen build action: destructive kinds route
    /// through a confirm overlay showing the literal command; the rest start
    /// immediately.
    pub(super) fn queue_build_action(&mut self, kind: BuildKind) {
        if kind == BuildKind::Clean {
            self.overlay = Some(Overlay::ConfirmBuild {
                kind,
                confirm: false,
            });
            return;
        }
        self.start_build(kind);
    }

    /// Starts `kind`'s command and moves the user to where its output
    /// streams. A failure to even compose or start the command is a log
    /// notice instead: the panel stays usable.
    pub(super) fn start_build(&mut self, kind: BuildKind) {
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
        let Some(command) = panel.command(kind, backend) else {
            self.logs.warn(format!(
                "{}: this backend offers no such action",
                kind.label()
            ));
            return;
        };
        let label = command.to_string();
        if !panel.start(kind, command, &mut self.processes) {
            return;
        }
        self.logs.info(format!("running {label}"));
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
