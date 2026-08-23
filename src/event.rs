//! Event source for the main loop.
//!
//! Today every event originates from the terminal, so a poll loop on the main
//! thread is enough --- no threads, no channels, no async (`AGENTS.md`:
//! "avoid premature async"). [`AppEvent`] is nonetheless a superset of
//! crossterm's events so that process and device events can be added as new
//! variants without reshaping the loop.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, ModifierKeyCode, MouseButton, MouseEvent,
    MouseEventKind,
};

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
    /// A mouse gesture the dashboard gives a meaning to: a left click or
    /// a wheel step. Reporting is opt-in (`[ui] mouse`,
    /// `crate::terminal::init`), and [`EventSource`] narrows the stream to
    /// exactly these kinds --- drag/motion reports fire per cell crossed
    /// and would flood the loop with nothing the UI answers.
    Mouse(MouseEvent),
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
                    // The one deliberate exception: a bare Ctrl release is
                    // how the shortcuts overlay (`App::handle_shortcuts_overlay_key`)
                    // knows the user let go without picking a letter. It only
                    // ever arrives when the terminal's Kitty keyboard protocol
                    // is on (`terminal::init`'s `PushKeyboardEnhancementFlags`),
                    // and only for the modifier key itself --- every other
                    // Release/Repeat stays discarded above, or every ordinary
                    // binding would fire twice once the protocol reports them.
                    Event::Key(key)
                        if key.kind == KeyEventKind::Release
                            && matches!(
                                key.code,
                                KeyCode::Modifier(
                                    ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl
                                )
                            ) =>
                    {
                        return Ok(AppEvent::Key(key));
                    }
                    // Bracketed paste arrives as one event instead of a
                    // storm of keypresses, which is what lets the shell
                    // tell a pasted command from a typed one.
                    Event::Paste(text) => return Ok(AppEvent::Paste(text)),
                    // Only the gestures with a meaning become events;
                    // everything else crossterm reports is dropped here, at
                    // the source, before it costs a dispatch.
                    Event::Mouse(mouse) if is_gesture(mouse.kind) => {
                        return Ok(AppEvent::Mouse(mouse));
                    }
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

/// The mouse kinds that ever become an [`AppEvent::Mouse`]: a left click
/// activates what it lands on and a wheel step scrolls. Motion and drag
/// reports arrive per cell crossed and have no meaning here, other buttons
/// and the release half of a click are not gestures the dashboard answers.
fn is_gesture(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::Down(MouseButton::Left)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn only_left_clicks_and_wheel_steps_are_gestures() {
        assert!(is_gesture(MouseEventKind::Down(MouseButton::Left)));
        assert!(is_gesture(MouseEventKind::ScrollUp));
        assert!(is_gesture(MouseEventKind::ScrollDown));
        // The noise half: per-cell motion/drag floods, other buttons,
        // releases, and the horizontal wheel nobody asks for here.
        assert!(!is_gesture(MouseEventKind::Down(MouseButton::Right)));
        assert!(!is_gesture(MouseEventKind::Down(MouseButton::Middle)));
        assert!(!is_gesture(MouseEventKind::Up(MouseButton::Left)));
        assert!(!is_gesture(MouseEventKind::Drag(MouseButton::Left)));
        assert!(!is_gesture(MouseEventKind::Moved));
        assert!(!is_gesture(MouseEventKind::ScrollLeft));
        assert!(!is_gesture(MouseEventKind::ScrollRight));
    }

    #[test]
    fn the_gesture_kinds_round_trip_through_the_variant() {
        let click = mouse(MouseEventKind::Down(MouseButton::Left));
        assert_eq!(AppEvent::Mouse(click), AppEvent::Mouse(click));
    }
}
