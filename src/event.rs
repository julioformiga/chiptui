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
            // A *due* tick outranks whatever is already queued: a
            // continuous input stream (a free-scrolling wheel reports
            // for seconds on end) never lets the poll time out, so the
            // tick-driven work --- spinners, the hotplug poll, the docs
            // debounce --- starves precisely while the user is watching
            // for signs of life. The queued event is not lost; the next
            // call reads it immediately.
            if self.last_tick.elapsed() >= self.tick_rate {
                self.last_tick = Instant::now();
                return Ok(AppEvent::Tick);
            }
            let timeout = self.tick_rate.saturating_sub(self.last_tick.elapsed());

            if event::poll(timeout)?
                && let Some(app_event) = to_app_event(event::read()?)
            {
                return Ok(app_event);
            }
        }
    }

    /// The next event that has *already arrived*, without blocking ---
    /// `None` when the queue is empty. Reports the loop discards
    /// (motion, repeats) are skipped here too, so a discarded report
    /// cannot end a batch early.
    fn try_next_event(&mut self) -> Result<Option<AppEvent>> {
        while event::poll(Duration::ZERO)? {
            if let Some(app_event) = to_app_event(event::read()?) {
                return Ok(Some(app_event));
            }
        }
        Ok(None)
    }

    /// One frame's events: the one [`Self::next_event`] blocked for, plus
    /// everything already queued behind it, in the order the loop should
    /// answer them. A burst then costs one redraw instead of one per
    /// event --- a free-scroll wheel reports hundreds of notches a
    /// second, each a no-op once a list's cursor sits at an end, and a
    /// redraw-per-event loop turns that burst into seconds of frozen UI
    /// that only clear after the whole backlog has been drawn through.
    ///
    /// The batch is capped ([`MAX_BATCH`]) because draining "everything
    /// queued" has no end of its own while the queue keeps refilling:
    /// without a ceiling a saturating source is the same freeze in
    /// another form, with no frame drawn at all. The leftovers are still
    /// queued, and the next call reads them.
    pub fn next_batch(&mut self) -> Result<Vec<AppEvent>> {
        let mut batch = Vec::with_capacity(4);
        batch.push(self.next_event()?);
        while batch.len() < MAX_BATCH {
            match self.try_next_event()? {
                Some(event) => batch.push(event),
                None => break,
            }
        }
        prioritize(&mut batch);
        Ok(batch)
    }
}

/// Events one [`EventSource::next_batch`] answers before the loop draws
/// again. High enough that an ordinary burst is one frame, low enough
/// that a source which never stops still gets drawn between batches.
const MAX_BATCH: usize = 256;

/// Wheel steps go to the back of the batch: a continuous gesture must
/// not bury the discrete input queued behind it --- a free-scroll wheel
/// reports for seconds, and every notch past a list's end is a no-op the
/// user's next keypress would be waiting on.
///
/// Only the *wheel* is demoted, not every mouse report. A left click is
/// discrete input like a key, and it carries the cursor and the focus a
/// key arriving right after it acts on: demoting clicks too would let
/// that key answer the row the click had not yet selected.
///
/// The sort is stable, so each half keeps its arrival order.
fn prioritize(batch: &mut [AppEvent]) {
    batch.sort_by_key(|event| matches!(event, AppEvent::Mouse(mouse) if is_wheel(mouse.kind)));
}

/// A wheel notch, as opposed to the click [`is_gesture`] also admits.
fn is_wheel(kind: MouseEventKind) -> bool {
    matches!(kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
}

/// One crossterm event to the [`AppEvent`] the loop answers, `None` for
/// the reports that die at the source --- the one mapping both the
/// blocking and the burst half of the loop share.
fn to_app_event(event: Event) -> Option<AppEvent> {
    match event {
        // Key *releases* and repeats are reported on some terminals;
        // acting on them would fire every binding twice.
        Event::Key(key) if key.kind == KeyEventKind::Press => Some(AppEvent::Key(key)),
        // The one deliberate exception: a bare Ctrl release is how the
        // shortcuts overlay (`App::handle_shortcuts_overlay_key`) knows
        // the user let go without picking a letter. It only ever arrives
        // when the terminal's Kitty keyboard protocol is on
        // (`terminal::init`'s `PushKeyboardEnhancementFlags`), and only
        // for the modifier key itself --- every other Release/Repeat
        // stays discarded above, or every ordinary binding would fire
        // twice once the protocol reports them.
        Event::Key(key)
            if key.kind == KeyEventKind::Release
                && matches!(
                    key.code,
                    KeyCode::Modifier(ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl)
                ) =>
        {
            Some(AppEvent::Key(key))
        }
        // Bracketed paste arrives as one event instead of a storm of
        // keypresses, which is what lets the shell tell a pasted command
        // from a typed one.
        Event::Paste(text) => Some(AppEvent::Paste(text)),
        // Only the gestures with a meaning become events; everything
        // else crossterm reports is dropped here, at the source, before
        // it costs a dispatch.
        Event::Mouse(mouse) if is_gesture(mouse.kind) => Some(AppEvent::Mouse(mouse)),
        Event::Resize(width, height) => Some(AppEvent::Resize { width, height }),
        _ => None,
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

    /// Wheel steps go to the back of a batch and every other report keeps
    /// its arrival order --- a click included, since it selects the row
    /// the key queued behind it acts on.
    #[test]
    fn a_batch_puts_the_wheel_behind_the_discrete_input() {
        let key = |c| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let wheel = || AppEvent::Mouse(mouse(MouseEventKind::ScrollDown));
        let click = || AppEvent::Mouse(mouse(MouseEventKind::Down(MouseButton::Left)));

        let mut batch = vec![wheel(), key('a'), wheel(), key('b'), wheel()];
        prioritize(&mut batch);
        assert_eq!(
            batch,
            vec![key('a'), key('b'), wheel(), wheel(), wheel()],
            "the keys jump the notches, both halves in arrival order"
        );

        // A click is discrete input: it stays where it arrived, so the
        // key behind it answers the row the click just selected.
        let mut batch = vec![click(), key('a')];
        prioritize(&mut batch);
        assert_eq!(batch, vec![click(), key('a')]);
    }

    /// The one mapping both halves of the loop share --- the contract the
    /// burst half (`try_next_event`) depends on just as much as the
    /// blocking one: the reports that die at the source die there in
    /// *both*, so a discarded report cannot end an event batch early.
    #[test]
    fn only_meaningful_reports_become_events() {
        let press = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let release = |code| {
            Event::Key(KeyEvent::new_with_kind(
                code,
                KeyModifiers::CONTROL,
                KeyEventKind::Release,
            ))
        };

        // Presses pass through, releases do not --- except the bare Ctrl
        // release the shortcuts overlay listens for.
        assert!(matches!(
            to_app_event(press(KeyCode::Char('a'))),
            Some(AppEvent::Key(_))
        ));
        assert_eq!(to_app_event(release(KeyCode::Char('a'))), None);
        let ctrl = release(KeyCode::Modifier(ModifierKeyCode::LeftControl));
        assert!(matches!(to_app_event(ctrl), Some(AppEvent::Key(_))));

        assert!(matches!(
            to_app_event(Event::Paste("cmd".into())),
            Some(AppEvent::Paste(_))
        ));
        assert!(matches!(
            to_app_event(Event::Mouse(mouse(MouseEventKind::ScrollDown))),
            Some(AppEvent::Mouse(_))
        ));
        // Motion is the flood the burst half must skip without giving up
        // on the events queued behind it.
        assert_eq!(
            to_app_event(Event::Mouse(mouse(MouseEventKind::Moved))),
            None
        );
        assert!(matches!(
            to_app_event(Event::Resize(80, 32)),
            Some(AppEvent::Resize { .. })
        ));
    }
}
