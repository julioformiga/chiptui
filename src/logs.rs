//! In-memory log/status buffer backing the log pane.
//!
//! Timestamps are relative to application start (monotonic `Instant`), which
//! avoids a date/time dependency and is the more useful reading for "how long
//! did that build take".

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Entries kept before the oldest are dropped. Sized so a verbose build's
/// output stays scrollable without growing unbounded.
pub const DEFAULT_CAPACITY: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warn,
    Error,
}

impl Level {
    /// Single-character gutter marker.
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Info => "·",
            Self::Success => "✓",
            Self::Warn => "!",
            Self::Error => "✗",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: Level,
    /// Time since application start.
    pub at: Duration,
    pub message: String,
}

pub struct LogStore {
    entries: VecDeque<LogEntry>,
    capacity: usize,
    started: Instant,
    /// Lines scrolled up from the bottom. Zero means "following the tail".
    scroll: usize,
}

impl LogStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(256)),
            capacity,
            started: Instant::now(),
            scroll: 0,
        }
    }

    pub fn push(&mut self, level: Level, message: impl Into<String>) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(LogEntry {
            level,
            at: self.started.elapsed(),
            message: message.into(),
        });

        if self.scroll > 0 {
            // Scroll counts back from the tail, so a new entry would otherwise
            // shift the view. Hold it still while the user reads older output,
            // clamped so at least the oldest surviving entry stays reachable.
            self.scroll = (self.scroll + 1).min(self.entries.len().saturating_sub(1));
        }
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.push(Level::Info, message);
    }

    pub fn success(&mut self, message: impl Into<String>) {
        self.push(Level::Success, message);
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        self.push(Level::Warn, message);
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.push(Level::Error, message);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Whether the view is pinned to the newest entry.
    pub fn is_following(&self) -> bool {
        self.scroll == 0
    }

    /// Scrolls towards older entries, stopping at the oldest one on screen.
    pub fn scroll_up(&mut self, lines: usize, viewport: usize) {
        let max = self.entries.len().saturating_sub(viewport);
        self.scroll = (self.scroll + lines).min(max);
    }

    /// Scrolls towards newer entries; reaching zero resumes following.
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    /// The `viewport` entries currently visible, oldest first.
    pub fn visible(&self, viewport: usize) -> impl Iterator<Item = &LogEntry> {
        let end = self.entries.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(viewport);
        self.entries.range(start..end)
    }
}

impl Default for LogStore {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(count: usize) -> LogStore {
        let mut store = LogStore::new(DEFAULT_CAPACITY);
        for i in 0..count {
            store.info(format!("line {i}"));
        }
        store
    }

    fn visible_messages(store: &LogStore, viewport: usize) -> Vec<&str> {
        store
            .visible(viewport)
            .map(|e| e.message.as_str())
            .collect()
    }

    #[test]
    fn shows_the_newest_entries_while_following() {
        let store = store_with(10);
        assert!(store.is_following());
        assert_eq!(
            visible_messages(&store, 3),
            vec!["line 7", "line 8", "line 9"]
        );
    }

    #[test]
    fn scrolling_is_clamped_to_the_buffer() {
        let mut store = store_with(10);
        store.scroll_up(100, 3);
        assert_eq!(store.scroll(), 7);
        assert_eq!(
            visible_messages(&store, 3),
            vec!["line 0", "line 1", "line 2"]
        );

        store.scroll_down(100);
        assert!(store.is_following());
    }

    #[test]
    fn a_viewport_larger_than_the_buffer_shows_everything() {
        let store = store_with(2);
        assert_eq!(visible_messages(&store, 50), vec!["line 0", "line 1"]);
    }

    #[test]
    fn oldest_entries_are_dropped_at_capacity() {
        let mut store = LogStore::new(3);
        for i in 0..5 {
            store.info(format!("line {i}"));
        }
        assert_eq!(store.len(), 3);
        assert_eq!(
            visible_messages(&store, 10),
            vec!["line 2", "line 3", "line 4"]
        );
    }

    #[test]
    fn new_entries_do_not_move_a_scrolled_view() {
        let mut store = store_with(4);
        store.scroll_up(2, 2);
        assert_eq!(visible_messages(&store, 2), vec!["line 0", "line 1"]);

        store.info("line 4");
        assert_eq!(
            visible_messages(&store, 2),
            vec!["line 0", "line 1"],
            "output arriving while scrolled back must not shift the view"
        );
    }

    #[test]
    fn eviction_cannot_scroll_past_the_oldest_surviving_entry() {
        let mut store = LogStore::new(4);
        for i in 0..4 {
            store.info(format!("line {i}"));
        }
        store.scroll_up(2, 2); // showing line 0, line 1

        store.info("line 4"); // capacity reached: line 0 is gone
        assert_eq!(store.scroll(), 3);
        assert_eq!(
            visible_messages(&store, 2),
            vec!["line 1"],
            "the view holds its position; the evicted entry simply disappears"
        );
    }

    #[test]
    fn empty_store_renders_nothing() {
        let store = LogStore::default();
        assert!(store.is_empty());
        assert_eq!(visible_messages(&store, 10), Vec::<&str>::new());
    }
}
