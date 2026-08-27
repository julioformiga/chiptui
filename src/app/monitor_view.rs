//! Row 3's Monitor tab: which console it shows, whether one is live, and
//! where the view sits in it.
//!
//! The scroll is anchored to the *top* of the document so live output never
//! shifts a scrolled view (`CLAUDE.md`); the four consoles share this one
//! offset, and the Terminal tab reuses the state but not the renderer.

use super::{App, Focus, LogTab};

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

    /// Byte offset of the device monitor's cursor within its current (last)
    /// line, for the renderer to draw where typed text will land. `None`
    /// unless the session owns the keyboard ([`Self::is_monitor_active`]),
    /// so no cursor is drawn once it exits or the user tabs away.
    pub fn monitor_cursor(&self) -> Option<usize> {
        self.is_monitor_active()
            .then(|| self.monitor_console.cursor())
    }

    /// Switches the Monitor tab's feed and re-pins it to the new output's
    /// tail --- a fresh session must not inherit the previous one's scroll.
    pub fn set_monitor_source(&mut self, source: MonitorSource) {
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
        if self.monitor_scroll.following {
            return;
        }
        let max = self.monitor_max_offset();
        self.monitor_scroll.offset = (self.monitor_scroll.offset + rows).min(max);
        self.monitor_scroll.following = self.monitor_scroll.offset >= max;
    }
}
