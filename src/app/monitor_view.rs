//! Row 3's Monitor tab: which console it shows, whether one is live, and
//! where the view sits in it.
//!
//! The scroll is anchored to the *top* of the document so live output never
//! shifts a scrolled view; the four consoles share this one
//! offset, and the Terminal tab reuses the state but not the renderer.

use super::{App, Focus, LogTab};
use ratatui::{buffer::Buffer, layout::Rect};

/// The visible content snapshot is frozen only while dragging. Coordinates
/// are cells, not byte offsets, so wide and combining characters stay intact.
#[derive(Default)]
pub(crate) struct MonitorSelection {
    pub snapshot: Option<Buffer>,
    pub anchor: Option<(u16, u16)>,
    pub end: (u16, u16),
}

impl MonitorSelection {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        let a = (anchor.1, anchor.0);
        let b = (self.end.1, self.end.0);
        (a.min(b)..=a.max(b)).contains(&(y, x))
    }

    pub fn text(&self) -> String {
        let Some(buffer) = &self.snapshot else {
            return String::new();
        };
        let mut lines = Vec::new();
        for y in buffer.area.y..buffer.area.bottom() {
            let mut line = String::new();
            let mut selected = false;
            let mut covered_until = buffer.area.x;
            for x in buffer.area.x..buffer.area.right() {
                if x < covered_until {
                    continue;
                }
                let symbol = buffer[(x, y)].symbol();
                let width = ratatui::text::Span::raw(symbol).width().max(1) as u16;
                covered_until = x.saturating_add(width);
                if (x..covered_until).any(|column| self.contains(column, y)) {
                    selected = true;
                    line.push_str(symbol);
                }
            }
            if selected {
                lines.push(line.trim_end().to_string());
            }
        }
        lines.join("\n")
    }

    pub fn clamp(area: Rect, point: (u16, u16)) -> (u16, u16) {
        (
            point.0.clamp(area.x, area.right() - 1),
            point.1.clamp(area.y, area.bottom() - 1),
        )
    }
}

/// Which live feed the Monitor tab is currently showing. Changed only at
/// explicit transition points --- never derived from [`FlashPanel`]/device
/// state each frame --- so a finished flash run's output stays visible until
/// the user deliberately starts the device monitor, instead of quietly
/// reverting the moment the run ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorSource {
    #[default]
    Device,
    Flash,
    /// A `mpremote run` session, spawned in a PTY so Ctrl+C can send a
    /// KeyboardInterrupt to the device.
    Run,
    /// A backend build command (`west build`), streamed like the flash
    /// commands but keyed to its own output buffer.
    Build,
}

/// Scroll state for the Monitor tab. Unlike the Log pane (whose scroll counts
/// back from the tail, holding the view as output arrives), the monitor
/// anchors `offset` to the **top** of its content: live output grows the
/// document downward, so a scrolled view holds without compensation, and
/// `following` re-pins to the tail exactly like `LogStore::is_following`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorScroll {
    pub following: bool,
    /// First visible visual (post-wrap) row; meaningful while scrolled.
    pub offset: usize,
}

impl Default for MonitorScroll {
    fn default() -> Self {
        Self {
            following: true,
            offset: 0,
        }
    }
}

/// Row metrics of the Monitor console currently on screen, published by the
/// renderer each frame (mirrors [`App::log_viewport`]) so key handlers clamp
/// to what is actually drawn. `rows` counts visual (post-wrap) lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MonitorView {
    pub rows: usize,
    pub viewport: usize,
    pub width: usize,
}

impl App {
    pub(super) fn set_device_pane_error(&mut self, message: impl Into<String>) {
        if let Some(browser) = &mut self.browser {
            browser.set_device_error(message);
        }
    }

    /// Whether an interactive device REPL/monitor session is currently
    /// eating every keystroke --- shared by [`App::on_key`] (to route bytes
    /// into the pty instead of dashboard navigation) and [`App::shortcuts`]
    /// (so the footer stops advertising bindings that cannot fire while this
    /// is true).
    pub(super) fn is_monitor_active(&self) -> bool {
        self.focus == Focus::Logs
            && self.log_tab == LogTab::Monitor
            && self.monitor_source == MonitorSource::Device
            && self.device_monitor_process.is_some()
    }

    /// Whether the Monitor tab is showing the run output and the run process
    /// is still alive --- Ctrl+C is intercepted here to send a
    /// KeyboardInterrupt (0x03) to the device instead of quitting.
    pub(super) fn is_run_active(&self) -> bool {
        self.focus == Focus::Logs
            && self.log_tab == LogTab::Monitor
            && self.monitor_source == MonitorSource::Run
            && self.run_process.is_some()
    }

    /// Whether the Monitor tab is currently showing run output (regardless of
    /// whether the process is still running).
    pub(super) fn is_run_view(&self) -> bool {
        self.focus == Focus::Logs
            && self.log_tab == LogTab::Monitor
            && self.monitor_source == MonitorSource::Run
    }

    /// The device monitor's cursor in its VT grid. It is visible only while
    /// that session owns the keyboard and the child did not hide it.
    pub fn monitor_cursor(&self) -> Option<(u16, u16)> {
        let screen = self.device_monitor_terminal.screen();
        (self.is_monitor_active() && !screen.hide_cursor()).then(|| screen.cursor_position())
    }

    /// Switches the Monitor tab's feed and re-pins it to the new output's
    /// tail --- a fresh session must not inherit the previous one's scroll.
    pub fn set_monitor_source(&mut self, source: MonitorSource) {
        self.monitor_selection.clear();
        self.monitor_source = source;
        self.monitor_scroll = MonitorScroll::default();
    }

    /// The highest first-visible row of the Monitor console, from the
    /// renderer-published geometry.
    pub(super) fn monitor_max_offset(&self) -> usize {
        self.monitor_view
            .rows
            .saturating_sub(self.monitor_view.viewport)
    }

    /// Scrolls the Monitor tab towards older output, leaving the tail.
    pub fn monitor_scroll_up(&mut self, rows: usize) {
        self.monitor_selection.clear();
        let max = self.monitor_max_offset();
        self.monitor_scroll.offset = if self.monitor_scroll.following {
            max.saturating_sub(rows)
        } else {
            self.monitor_scroll.offset.saturating_sub(rows)
        };
        self.monitor_scroll.following = false;
    }

    /// Scrolls the Monitor tab towards newer output; reaching the bottom
    /// resumes following.
    pub fn monitor_scroll_down(&mut self, rows: usize) {
        self.monitor_selection.clear();
        if self.monitor_scroll.following {
            return;
        }
        let max = self.monitor_max_offset();
        self.monitor_scroll.offset = (self.monitor_scroll.offset + rows).min(max);
        self.monitor_scroll.following = self.monitor_scroll.offset >= max;
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    #[test]
    fn reverse_selection_preserves_wide_and_combining_characters() {
        let area = Rect::new(3, 4, 12, 2);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(3, 4, "a界e\u{301}z", ratatui::style::Style::default());
        buffer.set_string(3, 5, "last", ratatui::style::Style::default());
        let selection = MonitorSelection {
            snapshot: Some(buffer),
            anchor: Some((6, 5)),
            end: (5, 4),
        };
        assert_eq!(selection.text(), "界e\u{301}z\nlast");
    }
}
