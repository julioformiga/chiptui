//! Event source for the main loop.
//!
//! Today every event originates from the terminal, so a poll loop on the main
//! thread is enough --- no threads, no channels, no async (`AGENTS.md`:
//! "avoid premature async"). [`AppEvent`] is nonetheless a superset of
//! crossterm's events so that process and device events can be added as new
//! variants without reshaping the loop.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};

use crate::error::Result;

/// Cadence of [`AppEvent::Tick`] when the terminal is idle.
pub const DEFAULT_TICK_RATE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    Key(KeyEvent),
    /// A block of text pasted into the terminal, delivered whole because
    /// bracketed paste is enabled (`crate::terminal::init`). Only the
    /// Terminal tab's shell wants it --- everywhere else a paste is not a
    /// gesture the dashboard has a meaning for.
    Paste(String),
    Resize {
        width: u16,
        height: u16,
    },
    /// Idle heartbeat: drives spinners and, later, process polling.
    Tick,
    /// Output or completion of an external command. Produced by
    /// [`crate::process::ProcessManager`], not by the terminal.
    Process(crate::process::ProcessEvent),
    /// A board-docs fetch finished (the pickers' online enrichment).
    /// Produced by [`crate::board_docs::BoardDocs`]' worker threads, drained
    /// by the binary's loop beside the process events.
    Docs(crate::board_docs::DocsEvent),
}

pub struct EventSource {
    tick_rate: Duration,
    last_tick: Instant,
}

impl EventSource {
    pub fn new(tick_rate: Duration) -> Self {
        Self {
            tick_rate,
            last_tick: Instant::now(),
        }
    }

    /// Blocks until the next event, or until the tick deadline passes.
    pub fn next_event(&mut self) -> Result<AppEvent> {
        loop {
            let timeout = self.tick_rate.saturating_sub(self.last_tick.elapsed());

            if event::poll(timeout)? {
                match event::read()? {
                    // Key *releases* and repeats are reported on some terminals;
                    // acting on them would fire every binding twice.
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        return Ok(AppEvent::Key(key));
                    }
                    // Bracketed paste arrives as one event instead of a
                    // storm of keypresses, which is what lets the shell
                    // tell a pasted command from a typed one.
                    Event::Paste(text) => return Ok(AppEvent::Paste(text)),
                    Event::Resize(width, height) => return Ok(AppEvent::Resize { width, height }),
                    _ => continue,
                }
            }

            if self.last_tick.elapsed() >= self.tick_rate {
                self.last_tick = Instant::now();
                return Ok(AppEvent::Tick);
            }
        }
    }
}

impl Default for EventSource {
    fn default() -> Self {
        Self::new(DEFAULT_TICK_RATE)
    }
}
