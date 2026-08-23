//! Terminal setup and teardown.
//!
//! `AGENTS.md`: the terminal must be restored on *every* exit path. Three
//! mechanisms cover them:
//!
//! * [`TerminalGuard::restore`] --- the normal path, which can report errors;
//! * `Drop` --- early returns and `?` propagation;
//! * a panic hook --- so a panic leaves a usable shell instead of a raw-mode
//!   terminal with a hidden cursor.

use std::io::{self, Stdout, Write};
use std::sync::Once;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::crossterm::tty::IsTty;

use crate::error::{Error, Result};

static PANIC_HOOK: Once = Once::new();

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// The Kitty keyboard protocol flags the shortcuts overlay needs: a bare
/// Ctrl press/release only ever arrives as its own `KeyCode::Modifier`
/// event when *both* `DISAMBIGUATE_ESCAPE_CODES` and
/// `REPORT_ALL_KEYS_AS_ESCAPE_CODES` are on (crossterm's own requirement,
/// not just `REPORT_EVENT_TYPES`), and `REPORT_EVENT_TYPES` is what makes
/// the release half of that pair exist at all. Only terminals that
/// implement the protocol honour this (kitty, ghostty, wezterm, foot,
/// alacritty, ...) --- `App::keyboard_enhanced` is what the shortcuts
/// overlay checks before relying on it; everywhere else falls back to the
/// `ctrl+k` toggle, which needs none of this.
const KEYBOARD_ENHANCEMENT_FLAGS: KeyboardEnhancementFlags = KeyboardEnhancementFlags::union(
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    KeyboardEnhancementFlags::union(
        KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
        KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
    ),
);

/// Owns the terminal's modified state and undoes it exactly once.
pub struct TerminalGuard {
    terminal: Tui,
    restored: bool,
    /// Whether this terminal answered the Kitty keyboard protocol probe ---
    /// fixed for the session, read once by `App::set_keyboard_enhanced` at
    /// startup and re-applied by `suspend` every time raw mode is re-entered
    /// (a `$EDITOR`/menuconfig hand-off drops it like everything else raw
    /// mode owns).
    keyboard_enhanced: bool,
    /// Whether mouse capture is on for this session --- set once by
    /// [`init`] from `[ui] mouse` and re-applied by `suspend` every time
    /// raw mode is re-entered, the same way `keyboard_enhanced` is: a
    /// `$EDITOR`/menuconfig hand-off drops capture with everything else
    /// raw mode owns (`restore_raw` always sends `DisableMouseCapture`).
    mouse: bool,
}

/// Enters raw mode and the alternate screen.
///
/// Mouse capture is opt-in (`mouse = true`, from `[ui] mouse` in the user
/// config): `SPEC.md` §11 makes keyboard primary, and leaving reporting off
/// keeps the terminal's own selection/scrollback working. `restore_raw`
/// disables capture unconditionally on every teardown path, so a panic or
/// early exit can never leave the terminal reporting mouse to the shell.
pub fn init(mouse: bool) -> Result<TerminalGuard> {
    if !io::stdout().is_tty() {
        return Err(Error::NotATerminal);
    }

    install_panic_hook();

    enable_raw_mode()?;
    let mut stdout = io::stdout();

    // Probed once, right after raw mode --- the same point crossterm's own
    // example does it. A `false` here is the common case (most Linux
    // terminals): the shortcuts overlay then relies on `ctrl+k` alone.
    let keyboard_enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    if keyboard_enhanced
        && let Err(source) = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KEYBOARD_ENHANCEMENT_FLAGS)
        )
    {
        let _ = disable_raw_mode();
        return Err(source.into());
    }

    // Bracketed paste makes a pasted block arrive as one event rather than
    // a storm of keypresses --- the Terminal tab's shell needs the
    // distinction (zsh runs every newline in an unbracketed paste).
    if let Err(source) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, Hide) {
        // Undo the half-applied setup before reporting.
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = disable_raw_mode();
        return Err(source.into());
    }

    // Mouse capture is asked for *after* the alternate screen: reporting
    // on makes the terminal send clicks to the app instead of selecting
    // text, so it rides the same opt-in flag and is undone with the same
    // half-applied-setup care as the block above.
    if mouse && let Err(source) = execute!(stdout, EnableMouseCapture) {
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(stdout, LeaveAlternateScreen, DisableBracketedPaste, Show);
        let _ = disable_raw_mode();
        return Err(source.into());
    }

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    Ok(TerminalGuard {
        terminal,
        restored: false,
        keyboard_enhanced,
        mouse,
    })
}

impl TerminalGuard {
    pub fn terminal(&mut self) -> &mut Tui {
        &mut self.terminal
    }

    /// Whether the shortcuts overlay's bare-Ctrl gesture is live on this
    /// terminal --- see [`App::set_keyboard_enhanced`](crate::app::App).
    pub fn keyboard_enhanced(&self) -> bool {
        self.keyboard_enhanced
    }

    /// Whether mouse capture is on for this session --- the same `[ui]
    /// mouse` answer `init` was given, kept so the app can mirror it
    /// (`App::set_mouse_enabled`) without re-reading the config.
    pub fn mouse(&self) -> bool {
        self.mouse
    }

    /// Restores the terminal, reporting failures. Safe to call more than once.
    pub fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        restore_raw()?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Leaves the alternate screen for the duration of `run`, so an
    /// interactive child --- `$EDITOR`, from the file viewer --- gets the
    /// real terminal instead of drawing into ChipTUI's own buffer, then
    /// re-enters it. The same "give up raw mode cleanly, always restore"
    /// shape `AGENTS.md` §6 asks of REPL/monitor sessions, applied to a
    /// one-shot child rather than a streamed one.
    ///
    /// `run` itself is infallible here on purpose: a failure to spawn or a
    /// non-zero exit from the child is the caller's concern (it belongs in
    /// the log, not as a fatal terminal error), so `run` should catch its own
    /// errors into `T` rather than short-circuit this method. Only failures
    /// to toggle the terminal itself are surfaced as `Err`.
    pub fn suspend<T>(&mut self, run: impl FnOnce() -> T) -> Result<T> {
        restore_raw()?;
        let value = run();
        enable_raw_mode()?;
        // `restore_raw` already popped the keyboard-enhancement flags (along
        // with everything else raw mode owns) --- re-push them before the
        // shortcuts overlay's bare-Ctrl gesture is expected to work again,
        // the same way every other piece of raw-mode state here is redone.
        if self.keyboard_enhanced {
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KEYBOARD_ENHANCEMENT_FLAGS)
            )?;
        }
        if self.mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
        }
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        self.terminal.clear()?;
        Ok(value)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Last line of defence: errors here cannot be reported anywhere useful.
        let _ = self.restore();
    }
}

/// Undoes the terminal modifications without needing the guard.
///
/// Used by the panic hook, where the guard is not reachable --- which is why
/// `PopKeyboardEnhancementFlags` is issued unconditionally rather than
/// guarded on `keyboard_enhanced`: a terminal that was never pushed the
/// protocol simply ignores the unrecognized escape sequence, and this path
/// has no `TerminalGuard` to ask.
fn restore_raw() -> io::Result<()> {
    let mut stdout = io::stdout();
    let leave = execute!(
        stdout,
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
    // Belt and suspenders: `Pop` asks the terminal to restore whatever sat
    // on its own keyboard-enhancement *stack* below the level chiptui
    // pushed --- which trusts the terminal's stack bookkeeping to be
    // correct. Traced live against a real terminal: pushing then popping
    // this exact flag set with nothing else involved (no chiptui, just raw
    // `printf`) left it stuck reporting every keystroke as a Kitty CSI-u
    // sequence, i.e. `Pop` alone does not reliably work. `CSI = 0 ; 1 u`
    // sets the *currently active* flags to zero directly, sidestepping
    // whatever the stack thinks it holds --- harmless on a terminal that
    // never turned the protocol on (an unrecognized escape sequence is
    // simply ignored, same as `Pop` above).
    let clear_kb_flags = write!(stdout, "\x1b[=0;1u");
    let raw = disable_raw_mode();
    stdout.flush()?;
    leave.and(clear_kb_flags).and(raw)
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_raw();
            previous(info);
        }));
    });
}

/// Puts `text` on the system clipboard by writing the terminal's own
/// clipboard escape (OSC 52) --- no subprocess, no dependency, and it
/// works over SSH where `xclip`-style tools cannot reach the local
/// clipboard. Support is the terminal's call (kitty, ghostty, wezterm,
/// alacritty and foot write it; tmux needs `set-clipboard on`; a terminal
/// that does not know the sequence ignores it, which leaves the log line
/// as the honest record of what was asked).
///
/// Written between frames, like any other escape: the sequence draws
/// nothing, so the next `draw` repaints over it untouched.
pub fn set_clipboard(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    stdout.flush()
}

/// Standard base64 (RFC 4648, with padding) over the ASCII payloads the
/// clipboard ever carries here --- small enough that a dependency would
/// cost more than these few lines.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let triple = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        // A MAC, the payload the clipboard actually carries.
        assert_eq!(base64(b"24:6F:28:AA:BB:CC"), "MjQ6NkY6Mjg6QUE6QkI6Q0M=");
    }
}
