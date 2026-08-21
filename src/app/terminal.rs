//! The Terminal tab: row 3's third tab, beside Log and Monitor, running the
//! user's own shell in a PTY.
//!
//! A local shell is a UI affordance, not a backend operation, so the tab is
//! not capability-gated and no backend knows about it. Entering the tab is
//! the whole start gesture (the monitor, by contrast, needs a port and so
//! waits for `m`): a shell has no prerequisite.
//!
//! The session follows the monitor's rules (`AGENTS.md` §6: interactive
//! sessions are not ordinary line-oriented output): spawned in a PTY, VT
//! output interpreted by [`crate::console::LineConsole`], and while the tab
//! is focused the shell owns the keyboard --- every keystroke becomes bytes
//! in the PTY, `ctrl+c` interrupts the shell's foreground command instead of
//! quitting ChipTUI, and the one escape is `ctrl+]` (the monitor's own
//! chord), which *detaches*: the shell keeps running and streaming into the
//! tab while the keyboard returns to the dashboard. Switching back to the
//! tab re-attaches. The shell ends the way shells end (`exit`, `ctrl+d`),
//! which frees the keyboard and leaves the transcript behind; entering the
//! tab again starts a fresh shell.

use std::path::Path;

use super::{App, Focus, LogTab};

/// How long a shell may run before the process manager kills it: a day,
/// matching the monitor session. A shell is interactive and long-lived by
/// nature --- the timeout only exists so a forgotten session cannot outlive
/// every cleanup path.
const SHELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(86400);

impl App {
    /// Whether the Terminal tab's shell owns the keyboard: the tab is
    /// focused, the shell is alive and attached. Shared by [`App::on_key`]
    /// (bytes go into the PTY instead of dashboard navigation),
    /// [`App::terminal_cursor`] and [`App::shortcuts`], exactly the role
    /// `is_monitor_active` plays for the device session.
    pub fn is_terminal_active(&self) -> bool {
        self.focus == Focus::Logs
            && self.log_tab == LogTab::Terminal
            && self.terminal_process.is_some()
            && !self.terminal_detached
    }

    /// Byte offset of the shell's cursor within its current (last) line, for
    /// the renderer to draw where typed text will land --- `None` unless the
    /// session owns the keyboard ([`Self::is_terminal_active`]).
    pub fn terminal_cursor(&self) -> Option<usize> {
        self.is_terminal_active()
            .then(|| self.terminal_console.cursor())
    }

    /// Points the Terminal tab at a command other than `$SHELL` --- how
    /// tests run a fake shell instead of the developer's real one.
    pub fn set_terminal_tool(&mut self, command: crate::process::Command) {
        self.terminal_tool = Some(command);
    }

    /// Shows the Terminal tab, starting a shell when none is alive and
    /// re-attaching the keyboard when one is (the state a `ctrl+]` detach
    /// left behind). Never moves focus: the ctrl chord flips this strip from
    /// panes that keep their cursor.
    pub fn show_terminal_tab(&mut self) {
        self.log_tab = LogTab::Terminal;
        self.terminal_detached = false;
        if self.terminal_process.is_none() {
            self.start_terminal_shell();
        }
    }

    /// The shell the tab runs: the test seam's command when one is set,
    /// else `$SHELL` --- `/bin/sh` when unset or empty.
    fn shell_program(&self) -> String {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string())
    }

    fn start_terminal_shell(&mut self) {
        let root = self
            .manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
        let command = match &self.terminal_tool {
            Some(tool) => tool.clone(),
            None => crate::process::Command::new(self.shell_program()).current_dir(root),
        };
        self.terminal_program = Path::new(command.program())
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| command.program())
            .to_string();

        // A new shell starts a new transcript, like `open_monitor` clears
        // the monitor's: the previous session's output belongs to its own
        // scroll, not to the shell that follows it.
        self.terminal_output.clear();
        self.terminal_console.reset();
        match self.processes.spawn_pty(command, SHELL_TIMEOUT) {
            Ok(id) => self.terminal_process = Some(id),
            Err(e) => {
                self.logs.error(format!("could not start the shell: {e}"));
                self.terminal_process = None;
            }
        }
    }
}
