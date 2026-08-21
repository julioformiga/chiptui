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
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::tty::IsTty;

use crate::error::{Error, Result};

static PANIC_HOOK: Once = Once::new();

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Owns the terminal's modified state and undoes it exactly once.
pub struct TerminalGuard {
    terminal: Tui,
    restored: bool,
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
    // Bracketed paste makes a pasted block arrive as one event rather than
    // a storm of keypresses --- the Terminal tab's shell needs the
    // distinction (zsh runs every newline in an unbracketed paste).
    if let Err(source) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, Hide) {
        // Undo the half-applied setup before reporting.
        let _ = disable_raw_mode();
        return Err(source.into());
    }

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    Ok(TerminalGuard {
        terminal,
        restored: false,
    })
}

impl TerminalGuard {
    pub fn terminal(&mut self) -> &mut Tui {
        &mut self.terminal
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
/// Used by the panic hook, where the guard is not reachable.
fn restore_raw() -> io::Result<()> {
    let mut stdout = io::stdout();
    let leave = execute!(
        stdout,
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
