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
use ratatui::crossterm::event::DisableMouseCapture;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
}

/// Enters raw mode and the alternate screen.
///
/// Mouse capture is *not* enabled: `SPEC.md` §11 makes keyboard primary, and
/// leaving mouse reporting off keeps the terminal's own selection/scrollback
/// working.
pub fn init() -> Result<TerminalGuard> {
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

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    Ok(TerminalGuard {
        terminal,
        restored: false,
        keyboard_enhanced,
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
    let raw = disable_raw_mode();
    stdout.flush()?;
    leave.and(raw)
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
