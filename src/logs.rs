//! In-memory log/status buffer backing the log pane.
//!
//! Timestamps are wall-clock, in the operator's local time zone, so entries
//! read against the system clock rather than time-since-launch. The offset
//! defaults to UTC and must be set once via [`LogStore::set_offset`] --- do
//! that at startup, before any other thread exists. `time`'s local-offset
//! lookup (`Cargo.toml`) reads the OS's `TZ` state via a C API that is
//! unsound to call once the process is multi-threaded, so it is done a
//! single time in `main` rather than per entry.
//!
//! Long messages are wrapped to the width the renderer publishes via
//! [`LogStore::set_view_width`], so an entry may span several visual lines.
//! Scroll positions and viewports count *visual* lines, not entries, which
//! is what keeps paging and clamping honest once entries wrap.

use std::collections::VecDeque;

use time::{OffsetDateTime, UtcOffset};

/// Entries kept before the oldest are dropped. Sized so a verbose build's
/// output stays scrollable without growing unbounded; configurable later
/// through a dedicated knob if operators need more history.
pub const DEFAULT_CAPACITY: usize = 1_000;

/// Columns the stamp ("HH:MM:SS.cc ") and level marker plus its space occupy
/// before an entry's message. Continuation rows of a wrapped entry are
/// indented by this much so they stay visually attached to their timestamp.
pub const PREFIX_WIDTH: usize = 14;

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
    /// Wall-clock time the entry was pushed, in `LogStore`'s configured offset.
    pub at: OffsetDateTime,
    pub message: String,
}

/// One visual line of a wrapped entry, as returned by
/// [`LogStore::visible_rows`].
#[derive(Debug, Clone)]
pub struct Row<'a> {
    /// The entry this row belongs to; the stamp and level come from it.
    pub entry: &'a LogEntry,
    /// This row's slice of the entry's message.
    pub text: String,
    /// Whether this is the entry's first visual line (the one with the stamp).
    pub first: bool,
}

pub struct LogStore {
    entries: VecDeque<LogEntry>,
    capacity: usize,
    offset: UtcOffset,
    /// Width entries wrap at, published by the renderer. Zero disables
    /// wrapping (one visual line per explicit newline).
    view_width: usize,
    /// Total wrapped lines across all entries --- the scroll clamp's and the
    /// scrollbar's content length. Maintained incrementally on push.
    total_lines: usize,
    /// Visual lines scrolled up from the bottom. Zero means "following the
    /// tail".
    scroll: usize,
}

impl LogStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(256)),
            capacity,
            offset: UtcOffset::UTC,
            view_width: 0,
            total_lines: 0,
            scroll: 0,
        }
    }

    /// Sets the offset applied to entries pushed from now on. See the module
    /// docs for why this must run once, at startup, before any other thread.
    pub fn set_offset(&mut self, offset: UtcOffset) {
        self.offset = offset;
    }

    /// The configured offset, for other subsystems that stamp wall-clock
    /// times (the build panel's report line).
    pub fn offset(&self) -> UtcOffset {
        self.offset
    }

    /// Publishes the width the pane wraps at (the renderer calls this each
    /// frame, before anything reads the view). A change re-wraps the buffer,
    /// so the cached line total --- and with it every scroll clamp --- stays
    /// matched to what is on screen.
    pub fn set_view_width(&mut self, width: usize) {
        if width != self.view_width {
            self.view_width = width;
            self.total_lines = self.entries.iter().map(|e| self.wrapped(e).len()).sum();
        }
    }

    pub fn push(&mut self, level: Level, message: impl Into<String>) {
        if self.entries.len() == self.capacity
            && let Some(evicted) = self.entries.pop_front()
        {
            self.total_lines -= self.wrapped(&evicted).len();
        }
        let entry = LogEntry {
            level,
            at: OffsetDateTime::now_utc().to_offset(self.offset),
            message: message.into(),
        };
        let added = self.wrapped(&entry).len();
        self.entries.push_back(entry);
        self.total_lines += added;

        if self.scroll > 0 {
            // Scroll counts back from the tail, so a new entry would otherwise
            // shift the view. Hold it still while the user reads older output,
            // by every visual line the entry added, clamped so at least one
            // line of the oldest surviving entry stays reachable.
            self.scroll = (self.scroll + added).min(self.total_lines.saturating_sub(1));
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

    /// Total wrapped lines: the scrollable content length, in visual lines.
    pub fn total_lines(&self) -> usize {
        self.total_lines
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Whether the view is pinned to the newest entry.
    pub fn is_following(&self) -> bool {
        self.scroll == 0
    }

    /// Scrolls towards older entries, stopping when the oldest line is on
    /// screen. `lines` and `viewport` are visual (wrapped) lines.
    pub fn scroll_up(&mut self, lines: usize, viewport: usize) {
        let max = self.total_lines.saturating_sub(viewport);
        self.scroll = (self.scroll + lines).min(max);
    }

    /// Scrolls towards newer entries; reaching zero resumes following.
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    /// The `[start, end)` window of visual lines currently on screen.
    fn window(&self, viewport: usize) -> (usize, usize) {
        let end = self.total_lines.saturating_sub(self.scroll);
        let start = end.saturating_sub(viewport);
        (start, end)
    }

    /// The entries with at least one visual line on screen, oldest first.
    pub fn visible(&self, viewport: usize) -> impl Iterator<Item = &LogEntry> {
        let (start, end) = self.window(viewport);
        let mut visible = Vec::new();
        let mut cursor = 0;
        for entry in &self.entries {
            let next = cursor + self.wrapped(entry).len();
            if next > start && cursor < end {
                visible.push(entry);
            }
            cursor = next;
        }
        visible.into_iter()
    }

    /// The visual lines currently on screen, oldest first, ready to render:
    /// each row knows whether it opens its entry (and so carries the stamp)
    /// or continues a wrapped one.
    pub fn visible_rows(&self, viewport: usize) -> Vec<Row<'_>> {
        let (start, end) = self.window(viewport);
        let mut rows = Vec::new();
        let mut cursor = 0;
        for entry in &self.entries {
            let lines = self.wrapped(entry);
            let next = cursor + lines.len();
            if next > start && cursor < end {
                let skip = start.saturating_sub(cursor);
                let take = end.min(next) - start.max(cursor);
                for (i, text) in lines.iter().skip(skip).take(take).enumerate() {
                    rows.push(Row {
                        entry,
                        text: text.clone(),
                        first: skip + i == 0,
                    });
                }
            }
            cursor = next;
            if cursor >= end {
                break;
            }
        }
        rows
    }

    /// The entry's message as the visual lines it occupies at the current
    /// width: wrapped to the budget left of the prefix, explicit newlines
    /// kept as row breaks.
    fn wrapped(&self, entry: &LogEntry) -> Vec<String> {
        let budget = self.view_width.saturating_sub(PREFIX_WIDTH);
        if budget == 0 {
            return entry.message.split('\n').map(String::from).collect();
        }
        entry
            .message
            .split('\n')
            .flat_map(|paragraph| wrap_text(paragraph, budget))
            .collect()
    }
}

/// Visual rows `text` occupies when wrapped to `width` columns. Shared with
/// the Monitor pane, whose scrollbar counts console lines the same way.
pub(crate) fn wrap_rows(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    text.split('\n')
        .map(|paragraph| wrap_text(paragraph, width).len())
        .sum()
}

/// Greedy word wrap of `text` to `width` columns, counted in characters.
///
/// Breaks at spaces; runs of spaces inside a line are preserved (build output
/// aligns with them) while spaces at a break are dropped. A word longer than
/// `width` (a path, a URL) is hard-broken rather than left to overflow.
/// Always returns at least one line. `width` must be greater than zero.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    debug_assert!(width > 0, "wrap_text requires a positive width");

    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize;
    let mut pending = 0usize; // spaces seen since the last word, not yet placed
    let mut first_line = true; // keep leading spaces of the text's own first line

    let mut rest = text;
    while !rest.is_empty() {
        let token_is_spaces = rest.starts_with(' ');
        let token_end = if token_is_spaces {
            rest.find(|c| c != ' ')
        } else {
            rest.find(' ')
        }
        .unwrap_or(rest.len());
        let token = &rest[..token_end];
        rest = &rest[token_end..];

        if token_is_spaces {
            pending += token.len();
            continue;
        }

        let word_len = token.chars().count();
        if line_len == 0 && !first_line {
            pending = 0; // a wrapped line does not inherit the break's spaces
        }
        if line_len + pending + word_len <= width {
            if pending > 0 {
                line.push_str(&" ".repeat(pending));
                line_len += pending;
                pending = 0;
            }
            line.push_str(token);
            line_len += word_len;
        } else if word_len <= width {
            // The word fits a line of its own: break before it.
            lines.push(std::mem::take(&mut line));
            line.push_str(token);
            line_len = word_len;
            pending = 0;
        } else {
            // The word alone exceeds the width: hard-break it, first
            // filling whatever room is left on the current line.
            if pending > 0 && line_len + pending < width {
                line.push_str(&" ".repeat(pending));
                line_len += pending;
            }
            pending = 0;
            let head_room = width.saturating_sub(line_len);
            let mut consumed = 0usize;
            if line_len > 0 {
                if head_room > 0 {
                    for ch in token.chars().take(head_room) {
                        line.push(ch);
                        consumed += 1;
                    }
                }
                lines.push(std::mem::take(&mut line));
            }
            let mut chunk = String::new();
            for ch in token.chars().skip(consumed) {
                chunk.push(ch);
                if chunk.chars().count() == width {
                    lines.push(std::mem::take(&mut chunk));
                }
            }
            line = chunk;
            line_len = line.chars().count();
        }
        first_line = false;
    }
    lines.push(line);
    lines
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

    fn row_texts(store: &LogStore, viewport: usize) -> Vec<(bool, String)> {
        store
            .visible_rows(viewport)
            .into_iter()
            .map(|r| (r.first, r.text))
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
        assert!(store.visible_rows(10).is_empty());
    }

    #[test]
    fn default_capacity_is_one_thousand_entries() {
        assert_eq!(DEFAULT_CAPACITY, 1_000);
    }

    #[test]
    fn long_messages_wrap_past_the_stamp_column() {
        let mut store = LogStore::new(DEFAULT_CAPACITY);
        // Ten columns of message fit next to the stamp.
        store.set_view_width(PREFIX_WIDTH + 10);
        store.info("aaaa bbbb cccc dddd");

        assert_eq!(store.total_lines(), 2);
        assert_eq!(
            row_texts(&store, 10),
            vec![(true, "aaaa bbbb".into()), (false, "cccc dddd".into())]
        );
    }

    #[test]
    fn explicit_newlines_are_row_breaks_within_one_entry() {
        let mut store = LogStore::new(DEFAULT_CAPACITY);
        store.set_view_width(PREFIX_WIDTH + 40);
        store.info("first\nsecond");

        let rows = row_texts(&store, 10);
        assert_eq!(
            rows,
            vec![(true, "first".into()), (false, "second".into())],
            "both rows belong to the same stamped entry"
        );
    }

    #[test]
    fn scrolling_and_clamping_count_wrapped_lines() {
        let mut store = LogStore::new(DEFAULT_CAPACITY);
        store.set_view_width(PREFIX_WIDTH + 10);
        store.info("aaaa bbbb cccc"); // two visual lines
        store.info("dddd eeee ffff"); // two visual lines
        assert_eq!(store.total_lines(), 4);

        store.scroll_up(usize::MAX, 2);
        assert_eq!(
            store.scroll(),
            2,
            "the clamp is total wrapped lines minus the viewport"
        );
        assert_eq!(
            row_texts(&store, 2),
            vec![(true, "aaaa bbbb".into()), (false, "cccc".into())]
        );
    }

    #[test]
    fn wrapped_output_arriving_while_scrolled_holds_the_view() {
        let mut store = LogStore::new(DEFAULT_CAPACITY);
        store.set_view_width(PREFIX_WIDTH + 10);
        store.info("aaaa bbbb cccc"); // two visual rows
        store.info("dddd"); // one visual row
        store.scroll_up(usize::MAX, 2); // scroll 1: showing "aaaa bbbb" and "cccc"
        assert_eq!(store.scroll(), 1);

        store.info("eeee ffff gggg"); // adds two visual rows below the view
        assert_eq!(store.scroll(), 3, "the view stays on the same rows");
        assert_eq!(
            row_texts(&store, 2),
            vec![(true, "aaaa bbbb".into()), (false, "cccc".into())]
        );
    }

    #[test]
    fn a_viewport_cutting_an_entry_in_half_shows_its_continuation() {
        let mut store = LogStore::new(DEFAULT_CAPACITY);
        store.set_view_width(PREFIX_WIDTH + 10);
        store.info("aaaa bbbb cccc"); // rows: "aaaa bbbb", "cccc"
        store.info("dddd"); // one row

        // Viewport of two while following: the tail starts at the second
        // row of the first entry, so its continuation opens the view.
        assert_eq!(
            row_texts(&store, 2),
            vec![(false, "cccc".into()), (true, "dddd".into())]
        );
    }

    #[test]
    fn changing_the_width_rewraps_and_reclamps() {
        let mut store = LogStore::new(DEFAULT_CAPACITY);
        store.set_view_width(PREFIX_WIDTH + 20);
        store.info("aaaa bbbb cccc dddd");
        assert_eq!(store.total_lines(), 1);

        store.scroll_up(5, 1); // no overflow yet: stays following
        assert!(store.is_following());

        store.set_view_width(PREFIX_WIDTH + 4);
        assert_eq!(store.total_lines(), 4, "one word per row at the new width");
    }

    #[test]
    fn wrap_breaks_at_spaces_and_preserves_inner_ones() {
        assert_eq!(wrap_text("aa  bb cc", 5), vec!["aa", "bb cc"]);
        assert_eq!(wrap_text("short", 20), vec!["short"]);
        assert_eq!(wrap_text("", 20), vec![""]);
        assert_eq!(wrap_text("   lead", 20), vec!["   lead"]);
        // Spaces at a break are dropped, not carried to the next line.
        assert_eq!(wrap_text("aa   bb", 3), vec!["aa", "bb"]);
    }

    #[test]
    fn wrap_hard_breaks_words_longer_than_the_width() {
        let path = "/very/long/path/without/spaces";
        let rows = wrap_text(path, 10);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|r| r.chars().count() <= 10));
        assert_eq!(rows.concat(), path, "hard breaks must not lose characters");

        // The room left on the current line is used before breaking.
        assert_eq!(wrap_text("ab cccccccc", 5), vec!["ab cc", "ccccc", "c"]);
    }
}
