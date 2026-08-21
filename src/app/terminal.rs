//! The Terminal tab: row 3's third tab, beside Log and Monitor, running the
//! user's own shell in a PTY.
//!
//! A local shell is a UI affordance, not a backend operation, so the tab is
//! not capability-gated and no backend knows about it. Entering the tab is
//! the whole start gesture (the monitor, by contrast, needs a port and so
//! waits for `m`): a shell has no prerequisite.
//!
//! The session follows the monitor's rules (`AGENTS.md` §6: interactive
//! sessions are not ordinary line-oriented output) --- but not its
//! *renderer*. The monitor's [`crate::console::LineConsole`] edits one line
//! and drops every attribute, which is exactly right for MicroPython's
//! readline redraw and exactly wrong for a shell: a real prompt
//! (powerlevel10k here) paints itself in 256 colours, moves the cursor up to
//! redraw its second row, and places a right-hand segment by column; a real
//! session switches to the alternate screen for `vim` or `less`. So this tab
//! owns a [`TerminalSession`] --- a `vt100` cell grid fed the PTY's raw
//! bytes --- and renders it with `tui-term`.
//!
//! While the tab is focused the shell owns the keyboard: every keystroke
//! becomes bytes in the PTY, `ctrl+c` interrupts the shell's foreground
//! command instead of quitting ChipTUI, and the one escape is `ctrl+]` (the
//! monitor's own chord), which *detaches*: the shell keeps running and
//! streaming into the tab while the keyboard returns to the dashboard.
//! Switching back to the tab re-attaches. The shell ends the way shells end
//! (`exit`, `ctrl+d`), which frees the keyboard and leaves the transcript
//! behind; entering the tab again starts a fresh shell.

use std::path::Path;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Focus, LogTab};

/// How long a shell may run before the process manager kills it: a day,
/// matching the monitor session. A shell is interactive and long-lived by
/// nature --- the timeout only exists so a forgotten session cannot outlive
/// every cleanup path.
const SHELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(86400);

/// Rows of scrollback the grid keeps behind the viewport, matching the log
/// store's own cap: a transcript that grows forever is a leak, and a session
/// that remembers nothing is not a terminal.
const SCROLLBACK: usize = 1_000;

/// The size a session opens at before a frame has been drawn. The renderer
/// replaces it with the pane's real geometry on the first draw
/// ([`App::resize_terminal`]); this only has to be sane enough that a shell
/// starting faster than the first frame does not compute a prompt against
/// nonsense.
const INITIAL_SIZE: (u16, u16) = (24, 80);

/// The half of the terminal `vt100` does not implement: the sequences a
/// child *asks a question* with.
///
/// `vt100` is a screen, not a terminal --- it has no way to write back, so
/// `CSI n` (device status) and `CSI c` (device attributes) fall through to
/// [`vt100::Callbacks::unhandled_csi`] and are dropped. A shell that asks
/// then waits forever, and asking is routine: a prompt that needs to know
/// which column it ended on sends `CSI 6 n` and blocks on the answer. So the
/// answers are composed here and drained into the PTY by
/// [`App::drain_terminal_replies`] right after the bytes that provoked them.
#[derive(Default)]
struct TerminalCallbacks {
    replies: Vec<u8>,
    title: Option<String>,
}

impl vt100::Callbacks for TerminalCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        action: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied();
        match (intermediate, action, first) {
            // DSR 5: "are you there?" --- yes, and no malfunction.
            (None, 'n', Some(5)) => self.replies.extend_from_slice(b"\x1b[0n"),
            // DSR 6 (CPR): where the cursor is, 1-based. Answered from the
            // screen handed in, so the reply describes the grid *now*.
            (None, 'n', Some(6)) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // DA1: claim a plain VT102 and nothing more. Over-claiming
            // invites a child to use a sequence `vt100` cannot honour;
            // under-claiming only costs an optional feature.
            (None, 'c', _) => self.replies.extend_from_slice(b"\x1b[?6c"),
            // DA2: a terminal identity with no version to boast about.
            (Some(b'>'), 'c', _) => self.replies.extend_from_slice(b"\x1b[>0;0;0c"),
            _ => {}
        }
    }
}

/// The Terminal tab's terminal: a `vt100` cell grid plus the size last
/// pushed to it *and* to the PTY, which is what makes
/// [`App::resize_terminal`] idempotent enough to call from every frame.
pub struct TerminalSession {
    parser: vt100::Parser<TerminalCallbacks>,
    size: (u16, u16),
}

impl Default for TerminalSession {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalSession {
    pub fn new() -> Self {
        let (rows, cols) = INITIAL_SIZE;
        Self {
            parser: Self::parser(rows, cols),
            size: INITIAL_SIZE,
        }
    }

    fn parser(rows: u16, cols: u16) -> vt100::Parser<TerminalCallbacks> {
        vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, TerminalCallbacks::default())
    }

    /// The grid, for the renderer and for the input encoder (which asks it
    /// which cursor-key and paste modes the child turned on).
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Feeds raw PTY bytes to the emulator. Raw is the point: decoding per
    /// read chunk would replace any character split across the boundary
    /// with U+FFFD, and a powerline separator is three bytes.
    pub fn feed(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    /// Writes text as if the child had printed it --- how the session's own
    /// `[shell ...]` epitaph lands in the grid.
    pub fn write(&mut self, text: &str) {
        self.parser.process(text.as_bytes());
    }

    /// Empties the grid and the scrollback: a new shell starts a new
    /// transcript, the way `open_monitor` clears the monitor's.
    pub fn reset(&mut self) {
        let (rows, cols) = self.size;
        self.parser = Self::parser(rows, cols);
    }

    /// Takes whatever the child's queries earned in reply, for the caller to
    /// put back into the PTY (see [`TerminalCallbacks`]).
    fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }

    /// The title the shell set (OSC 0/2). Powerlevel10k puts the working
    /// directory here, which is worth more on the tab strip than the
    /// program's name.
    pub fn title(&self) -> Option<&str> {
        self.parser.callbacks().title.as_deref()
    }

    /// Resizes the grid, reporting whether anything changed so the caller
    /// only troubles the child with a real SIGWINCH.
    fn resize(&mut self, rows: u16, cols: u16) -> bool {
        let size = (rows.max(1), cols.max(1));
        if size == self.size {
            return false;
        }
        self.size = size;
        self.parser.screen_mut().set_size(size.0, size.1);
        true
    }

    /// How far back the viewport is scrolled, in rows.
    pub fn scrollback(&self) -> usize {
        self.screen().scrollback()
    }

    /// How many rows of history sit above the viewport.
    ///
    /// `vt100` exposes the scrollback *position* but not its length, and
    /// clamps a position it cannot reach --- so asking for an impossible
    /// one and reading back what was granted is the measurement. The
    /// position is restored before returning, which makes this a query
    /// despite the `&mut`.
    pub fn scrollback_len(&mut self) -> usize {
        if self.screen().alternate_screen() {
            // A full-screen program owns the viewport; there is no history
            // behind it to scroll into.
            return 0;
        }
        let current = self.scrollback();
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let len = self.screen().scrollback();
        self.parser.screen_mut().set_scrollback(current);
        len
    }

    /// Scrolls the viewport back through the history. A full-screen program
    /// owns its viewport, so the alternate screen has no scrollback to
    /// reach --- and `vt100` would refuse anyway.
    pub fn set_scrollback(&mut self, rows: usize) {
        if self.screen().alternate_screen() {
            return;
        }
        self.parser.screen_mut().set_scrollback(rows);
    }
}

impl App {
    /// Whether the Terminal tab's shell owns the keyboard: the tab is
    /// focused, the shell is alive and attached. Shared by [`App::on_key`]
    /// (bytes go into the PTY instead of dashboard navigation), the
    /// renderer's cursor and [`App::shortcuts`], exactly the role
    /// `is_monitor_active` plays for the device session.
    pub fn is_terminal_active(&self) -> bool {
        self.focus == Focus::Logs
            && self.log_tab == LogTab::Terminal
            && self.terminal_process.is_some()
            && !self.terminal_detached
    }

    /// Where the shell's cursor sits in the grid, for the renderer to draw
    /// where typed text will land --- `None` unless the session owns the
    /// keyboard ([`Self::is_terminal_active`]) and the child left the
    /// cursor visible.
    pub fn terminal_cursor(&self) -> Option<(u16, u16)> {
        let screen = self.terminal.screen();
        (self.is_terminal_active() && !screen.hide_cursor()).then(|| screen.cursor_position())
    }

    /// Points the Terminal tab at a command other than `$SHELL` --- how
    /// tests run a fake shell instead of the developer's real one.
    pub fn set_terminal_tool(&mut self, command: crate::process::Command) {
        self.terminal_tool = Some(command);
    }

    /// Feeds the shell's raw output to the emulator and answers whatever it
    /// asked. The answer goes back in the *same* turn: a child that sent
    /// `CSI 6 n` is blocked on the reply, so deferring it to a tick would
    /// stall the shell for as long as the deferral lasts.
    pub(super) fn feed_terminal(&mut self, data: &[u8]) {
        self.terminal.feed(data);
        let replies = self.terminal.take_replies();
        if let Some(id) = self.terminal_process.filter(|_| !replies.is_empty()) {
            self.processes.write_stdin(id, &replies);
        }
    }

    /// Matches the emulator and the child to the pane the renderer just
    /// measured. Called every frame and a no-op unless the size actually
    /// changed: the shell recomputes its prompt on every SIGWINCH, so
    /// firing one per frame would make the prompt flicker forever.
    pub fn resize_terminal(&mut self, rows: u16, cols: u16) {
        if self.terminal.resize(rows, cols)
            && let Some(id) = self.terminal_process
        {
            self.processes.resize_pty(id, rows, cols);
        }
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

    /// The resolved Zephyr workspace's exported environment --- the same
    /// half the build panel's commands get through `set_tool_env`
    /// (`ZEPHYR_BASE` always; `ZEPHYR_SDK_INSTALL_DIR`, `VIRTUAL_ENV` and a
    /// venv-first `PATH` when the workspace owns them). The Terminal tab's
    /// shell is a UI affordance every backend offers, so there is no
    /// capability to gate on: no workspace resolved, no variables, and the
    /// shell inherits the parent's own environment untouched.
    fn terminal_west_env(&self) -> Vec<(String, String)> {
        self.workspace
            .as_ref()
            .map_or_else(Vec::new, |panel| panel.west_env().env)
    }

    pub(super) fn start_terminal_shell(&mut self) {
        let root = self
            .manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
        let west_env = self.terminal_west_env();
        let command = match &self.terminal_tool {
            Some(tool) => tool.clone(),
            None => crate::process::Command::new(self.shell_program())
                .current_dir(root)
                // A login shell, what a fresh terminal window starts: the
                // parent's own environment is inherited whole, but the
                // variables a login session exports (`PATH` additions,
                // pyenv and friends, set in `.zprofile`/`.profile`) reach
                // a shell only through its login files --- which plain
                // `$SHELL` never sources. portable-pty resolves the shell
                // itself on this path (`$SHELL`, then the passwd entry);
                // `shell_program` above stays the tab label's answer.
                .as_login_shell(),
        }
        // The workspace's environment rides along, so `west`, `python` and
        // `cmake` typed in the tab mean what they mean in the Actions pane
        // --- the whole point of handing the shell the developer's Zephyr
        // setup rather than the bare parent environment.
        .envs(west_env.clone())
        // `TERM` is a promise about what the *emulator* can do, and the one
        // this tab makes is `vt100`'s: an xterm-shaped, 256-colour terminal.
        // Inheriting the outer terminal's own value (`xterm-ghostty` here)
        // would advertise graphics and query protocols nothing behind this
        // tab can answer. `COLORTERM` stays, because truecolor really does
        // survive: `vt100` parses `38;2;r;g;b` and `tui-term` renders it.
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor");
        self.terminal_shell_env = west_env;

        self.terminal_program = Path::new(command.program())
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| command.program())
            .to_string();

        // A new shell starts a new transcript, like `open_monitor` clears
        // the monitor's: the previous session's output belongs to its own
        // scroll, not to the shell that follows it.
        self.terminal.reset();
        self.monitor_scroll = super::MonitorScroll::default();
        let (rows, cols) = self.terminal.size();
        match self
            .processes
            .spawn_pty_raw(command, SHELL_TIMEOUT, rows, cols)
        {
            Ok(id) => self.terminal_process = Some(id),
            Err(e) => {
                self.logs.error(format!("could not start the shell: {e}"));
                self.terminal_process = None;
            }
        }
    }

    /// Replaces a live shell session with one born under the environment
    /// the workspace resolves to *now*. A process cannot have its
    /// environment edited from outside, and a workspace that resolved or
    /// moved under the Terminal tab changes what `west` and `python` mean
    /// there --- so the stale session is ended and a fresh one started
    /// (the same trade `r` makes) rather than left quietly disagreeing
    /// with the Actions pane. The old shell's last events are dropped by
    /// the id match; the reset below belongs to the new session alone.
    pub(super) fn restart_terminal_shell(&mut self) {
        if let Some(id) = self.terminal_process.take() {
            self.processes.cancel(id);
        }
        self.start_terminal_shell();
        self.logs
            .info("terminal: shell restarted with the workspace's environment");
    }

    /// Sends a paste to the shell, framed the way the child asked for it.
    /// A child in bracketed-paste mode wants the block delimited so it can
    /// tell pasted text from typing (zsh, without it, runs every newline in
    /// a pasted command); one that never asked gets the bytes plain.
    pub fn paste_into_terminal(&mut self, text: &str) {
        let Some(id) = self.terminal_process else {
            return;
        };
        let mut bytes = Vec::new();
        let bracketed = self.terminal.screen().bracketed_paste();
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        self.processes.write_stdin(id, &bytes);
    }
}

/// Encodes a keystroke for a shell, which wants far more of the keyboard
/// than the device monitor does.
///
/// [`super::key_to_bytes`] stays as the monitor's encoder: mpremote's REPL
/// has no use for function keys, and widening it would start sending them.
/// This one is the terminal's, and covers what a shell's line editor binds:
/// Meta (every `alt+` chord zsh reads as `ESC` + key), the editing cluster
/// (Home/End/Insert/Delete/Page), the function row, and modified arrows in
/// xterm's `CSI 1 ; modifier final` form.
///
/// `application_cursor` is DECCKM, which the child turns on and off as it
/// pleases (zsh's line editor does, `vim` does): the same Up arrow must be
/// sent as `ESC O A` while it is set and `ESC [ A` while it is not.
pub fn terminal_key_bytes(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut bytes = encode_key(key, application_cursor)?;
    if alt && !matches!(key.code, KeyCode::Esc) {
        // Meta is a prefix, not a modifier byte: `alt+f` is `ESC f`. A
        // sequence that already carries its modifier numerically (see
        // `modifier_code`) has had it folded in and must not be prefixed
        // too --- those return early from `encode_key`.
        if bytes.first() != Some(&0x1b) {
            bytes.insert(0, 0x1b);
        }
    }
    Some(bytes)
}

fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let modifier = modifier_code(key.modifiers);
    let csi = |final_byte: u8| -> Vec<u8> {
        match modifier {
            1 => vec![0x1b, b'[', final_byte],
            m => format!("\x1b[1;{m}")
                .into_bytes()
                .into_iter()
                .chain([final_byte])
                .collect(),
        }
    };
    // `ESC [ n ~` keys carry their modifier as a second parameter.
    let tilde = |number: u8| -> Vec<u8> {
        match modifier {
            1 => format!("\x1b[{number}~").into_bytes(),
            m => format!("\x1b[{number};{m}~").into_bytes(),
        }
    };

    let bytes = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // The same relabelling `key_to_bytes` undoes: crossterm reports
            // the raw control bytes 0x00 and 0x1c..=0x1f as Ctrl+Space and
            // Ctrl+4..=Ctrl+7, which must be converted back rather than
            // XORed ('5' ^ 0x40 is 'u').
            let byte = match c {
                ' ' => 0x00,
                '4'..='7' => c as u8 - b'4' + 0x1c,
                _ => c.to_ascii_uppercase() as u8 ^ 0x40,
            };
            vec![byte]
        }
        KeyCode::Char(c) => {
            let mut buf = [0; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7F], // DEL
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        // Cursor keys follow DECCKM only while unmodified: a modified arrow
        // is always the `CSI 1 ; m` form, in either mode.
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left => {
            let final_byte = match key.code {
                KeyCode::Up => b'A',
                KeyCode::Down => b'B',
                KeyCode::Right => b'C',
                _ => b'D',
            };
            if modifier == 1 && application_cursor {
                vec![0x1b, b'O', final_byte]
            } else {
                csi(final_byte)
            }
        }
        KeyCode::Home => csi(b'H'),
        KeyCode::End => csi(b'F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let final_byte = b'P' + (n - 1);
            if modifier == 1 {
                vec![0x1b, b'O', final_byte]
            } else {
                csi(final_byte)
            }
        }
        KeyCode::F(n @ 5..=12) => {
            // xterm's numbering, which skips 16, 22, 27, 30 and 35.
            const NUMBERS: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            tilde(NUMBERS[(n - 5) as usize])
        }
        _ => return None,
    };
    Some(bytes)
}

/// xterm's modifier parameter: 1 plus a bitmask of shift(1), alt(2) and
/// ctrl(4), so `ctrl+shift+right` is `ESC [ 1 ; 6 C`.
fn modifier_code(modifiers: KeyModifiers) -> u8 {
    let mut mask = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        mask |= 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        mask |= 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        mask |= 4;
    }
    mask + 1
}

#[cfg(test)]
mod tests {
    use super::terminal_key_bytes;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn bytes(code: KeyCode, modifiers: KeyModifiers, application_cursor: bool) -> Vec<u8> {
        terminal_key_bytes(KeyEvent::new(code, modifiers), application_cursor)
            .expect("the shell encoder has a form for this key")
    }

    /// Meta is a prefix. `alt+f`/`alt+b` are zsh's word motions and
    /// `alt+backspace` is its delete-word --- all three were unreachable
    /// while the tab used the monitor's encoder, which never looked at the
    /// ALT modifier at all and sent the bare character instead.
    #[test]
    fn alt_prefixes_the_key_with_escape() {
        assert_eq!(
            bytes(KeyCode::Char('f'), KeyModifiers::ALT, false),
            b"\x1bf"
        );
        assert_eq!(
            bytes(KeyCode::Backspace, KeyModifiers::ALT, false),
            b"\x1b\x7f"
        );
        assert_eq!(bytes(KeyCode::Enter, KeyModifiers::ALT, false), b"\x1b\r");
    }

    /// DECCKM: the child decides which form its own arrows take, and zsh's
    /// line editor turns it on. Sending the wrong one is why arrow keys
    /// print `^[[A` in a naive emulator.
    #[test]
    fn arrows_follow_the_application_cursor_mode() {
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::NONE, false), b"\x1b[A");
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::NONE, true), b"\x1bOA");
        // A modified arrow is always the CSI form, in either mode.
        assert_eq!(
            bytes(KeyCode::Right, KeyModifiers::CONTROL, true),
            b"\x1b[1;5C"
        );
        assert_eq!(
            bytes(
                KeyCode::Right,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                false
            ),
            b"\x1b[1;6C"
        );
    }

    /// The editing cluster and the function row, in xterm's forms. None of
    /// these reached the shell before: `key_to_bytes` returns `None` for
    /// every one of them, so forward-delete was simply impossible.
    #[test]
    fn the_editing_and_function_keys_use_their_xterm_forms() {
        assert_eq!(
            bytes(KeyCode::Delete, KeyModifiers::NONE, false),
            b"\x1b[3~"
        );
        assert_eq!(
            bytes(KeyCode::Insert, KeyModifiers::NONE, false),
            b"\x1b[2~"
        );
        assert_eq!(
            bytes(KeyCode::PageUp, KeyModifiers::NONE, false),
            b"\x1b[5~"
        );
        assert_eq!(bytes(KeyCode::Home, KeyModifiers::NONE, false), b"\x1b[H");
        assert_eq!(bytes(KeyCode::End, KeyModifiers::NONE, false), b"\x1b[F");
        assert_eq!(
            bytes(KeyCode::BackTab, KeyModifiers::NONE, false),
            b"\x1b[Z"
        );
        assert_eq!(bytes(KeyCode::F(1), KeyModifiers::NONE, false), b"\x1bOP");
        assert_eq!(bytes(KeyCode::F(5), KeyModifiers::NONE, false), b"\x1b[15~");
        assert_eq!(
            bytes(KeyCode::F(12), KeyModifiers::NONE, false),
            b"\x1b[24~"
        );
    }

    /// The control relabel crossterm applies must be undone the same way
    /// the monitor's encoder undoes it: `ctrl+]` arrives as Ctrl+5 and is
    /// the byte 0x1d, not `'5' ^ 0x40`.
    #[test]
    fn control_bytes_survive_crossterms_relabelling() {
        assert_eq!(bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, false), [3]);
        assert_eq!(
            bytes(KeyCode::Char('5'), KeyModifiers::CONTROL, false),
            [0x1d]
        );
        assert_eq!(bytes(KeyCode::Char(' '), KeyModifiers::CONTROL, false), [0]);
    }
}
