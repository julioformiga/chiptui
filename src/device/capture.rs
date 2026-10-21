//! The device monitor's plain-text side capture.
//!
//! The visible monitor is a VT grid; this is the line-oriented copy the
//! board heuristics read ([`super::monitor_script_activity`],
//! [`super::micropython_version`]). A monitor session can run for a day, so
//! the capture keeps a bounded tail, and every line is read once, the moment
//! it completes, instead of the whole transcript being rescanned per chunk.

use super::{ScriptState, banner_version, is_meaningful, script_verdict, shows_prompt};
use crate::console::LineConsole;

/// Lines kept once the capture trims --- the VT grid's and the log's own
/// scrollback. The heuristics need none of them: their tallies already
/// cover every line that ever completed.
const KEEP: usize = 1_000;

/// The monitor's text, as [`LineConsole`] renders it, plus the tallies the
/// board heuristics need. Only the last line is ever edited (the console's
/// cursor lives there), so every earlier line is final and is counted
/// exactly once; the last line is read on demand.
#[derive(Debug, Default)]
pub struct MonitorCapture {
    lines: Vec<String>,
    console: LineConsole,
    /// Lines before this index are complete and already in the tallies.
    counted: usize,
    prompt_seen: bool,
    meaningful: usize,
    version: Option<String>,
}

impl MonitorCapture {
    /// Feeds one decoded chunk of monitor output.
    pub fn feed(&mut self, text: &str) {
        self.console.feed(&mut self.lines, text);
        self.settle();
    }

    /// Appends `text` as its own line (the session's epitaph).
    pub fn push_line(&mut self, text: String) {
        self.console.push_line(&mut self.lines, text);
        self.settle();
    }

    /// Forgets everything, for a new session.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The retained tail of the transcript, oldest first.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// [`super::monitor_script_activity`] over everything this session
    /// showed, trimmed lines included.
    pub fn script_activity(&self) -> Option<ScriptState> {
        let current = self.current_line();
        script_verdict(
            self.prompt_seen || shows_prompt(current),
            self.meaningful + usize::from(is_meaningful(current)),
        )
    }

    /// [`super::micropython_version`] over everything this session showed.
    pub fn micropython_version(&self) -> Option<String> {
        self.version
            .clone()
            .or_else(|| banner_version(self.current_line()))
    }

    /// The line still being written, which the tallies do not cover yet.
    fn current_line(&self) -> &str {
        self.lines.last().map_or("", String::as_str)
    }

    /// Counts the lines completed since the last call, then trims.
    fn settle(&mut self) {
        let complete = self.lines.len().saturating_sub(1);
        for line in &self.lines[self.counted..complete] {
            self.prompt_seen |= shows_prompt(line);
            self.meaningful += usize::from(is_meaningful(line));
            if self.version.is_none() {
                self.version = banner_version(line);
            }
        }
        self.counted = complete;
        // Twice the tail before trimming back to it, so the copy-down is
        // paid once per `KEEP` lines rather than once per line.
        if self.lines.len() > 2 * KEEP {
            let dropped = self.lines.len() - KEEP;
            self.lines.drain(..dropped);
            self.counted -= dropped;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{micropython_version, monitor_script_activity};
    use super::*;

    /// A connect, a prompt the user types at, and the edits a REPL echoes
    /// (backspace + erase-in-line), split at awkward points.
    const SESSION: &[&str] = &[
        "Connected to MicroPython at /dev/ttyACM0\r\n",
        "MicroPython v1.28.0 on 2026-08-14; ESP32",
        " board\r\nType \"help()\" for more information.\r\n>",
        ">> ",
        "pri\x08\x1b[Knt(1)\r\n1\r\n>>> ",
    ];

    /// A board that only ever prints: no banner, no prompt.
    const SCRIPT: &[&str] = &["temp: 21.3\r\ntemp: 2", "1.4\r\n", "temp: 21.4\r\n"];

    /// After every chunk, the running tallies answer exactly what a full
    /// rescan of the same lines answers --- the partial last line included.
    #[test]
    fn the_running_tallies_match_a_full_rescan_after_every_chunk() {
        for chunks in [SESSION, SCRIPT] {
            let mut capture = MonitorCapture::default();
            for chunk in chunks {
                capture.feed(chunk);
                assert_eq!(
                    capture.script_activity(),
                    monitor_script_activity(capture.lines()),
                    "after {chunk:?}: {:?}",
                    capture.lines()
                );
                assert_eq!(
                    capture.micropython_version(),
                    micropython_version(capture.lines()),
                    "after {chunk:?}: {:?}",
                    capture.lines()
                );
            }
        }
    }

    #[test]
    fn a_long_session_keeps_a_bounded_tail_and_its_verdicts() {
        let mut capture = MonitorCapture::default();
        capture.feed("MicroPython v1.28.0 on 2026-08-14; ESP32\r\n>>> \r\n");
        for index in 0..10 * KEEP {
            capture.feed(&format!("line {index}\r\n"));
        }
        assert!(
            capture.lines().len() <= 2 * KEEP,
            "{}",
            capture.lines().len()
        );
        assert_eq!(
            capture.lines().last().map(String::as_str),
            Some(""),
            "the tail is kept, up to the line being written"
        );
        // The banner and the prompt scrolled out of the tail long ago; what
        // they said still stands, as it would over the untrimmed transcript.
        assert_eq!(capture.micropython_version().as_deref(), Some("v1.28.0"));
        assert_eq!(capture.script_activity(), Some(ScriptState::Stopped));
    }

    #[test]
    fn clearing_starts_a_new_session() {
        let mut capture = MonitorCapture::default();
        capture.feed("MicroPython v1.28.0 on 2026-08-14; ESP32\r\n>>> ");
        capture.clear();
        assert!(capture.is_empty());
        assert_eq!(capture.script_activity(), None);
        assert_eq!(capture.micropython_version(), None);

        capture.feed("a\r\nb\r\nc\r\n");
        assert_eq!(capture.script_activity(), Some(ScriptState::Running));
    }

    #[test]
    fn the_epitaph_lands_on_its_own_line() {
        let mut capture = MonitorCapture::default();
        capture.feed(">>> ");
        capture.push_line("[monitor ok]".to_string());
        assert_eq!(capture.lines(), [">>> ", "[monitor ok]"]);
        assert_eq!(capture.script_activity(), Some(ScriptState::Stopped));
    }
}
