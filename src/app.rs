//! Application state and event handling.
//!
//! The loop is deliberately thin: [`App::handle`] maps an [`AppEvent`] to a
//! state change and returns; rendering is a pure function of the state
//! afterwards. Nothing here blocks, so adding long-running work later means
//! adding events, not restructuring this file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::backend::{BackendKind, Capability};
use crate::browser::{Browser, Side};
use crate::device::DeviceState;
use crate::error::Result;
use crate::event::AppEvent;
use crate::logs::LogStore;
use crate::process::ProcessManager;
use crate::project::{DetectionOutcome, DetectionSource, ProjectManager};

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    /// Dual-pane local/device file browser.
    Files,
}

/// Which pane receives navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Project,
    Capabilities,
    Logs,
}

impl Focus {
    const ORDER: [Focus; 3] = [Focus::Project, Focus::Capabilities, Focus::Logs];

    fn step(self, forward: bool) -> Self {
        let len = Self::ORDER.len();
        let index = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        Self::ORDER[next]
    }
}

/// A modal layer drawn above the panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    /// Manual backend selection (`AGENTS.md` §4: detection must be overridable).
    BackendPicker {
        selected: usize,
    },
    /// Serial device selection (`SPEC.md` §8: never guess which board).
    DevicePicker {
        selected: usize,
    },
}

/// One entry of the backend picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOption {
    /// Trust detection.
    Automatic,
    Backend(BackendKind),
}

impl PickerOption {
    pub fn all() -> Vec<PickerOption> {
        std::iter::once(PickerOption::Automatic)
            .chain(BackendKind::ALL.iter().copied().map(PickerOption::Backend))
            .collect()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic (use detection)",
            Self::Backend(kind) => kind.display_name(),
        }
    }
}

pub struct App {
    pub manager: ProjectManager,
    pub logs: LogStore,
    pub view: View,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    /// Cursor in the capabilities list.
    pub capability_cursor: usize,
    /// Height of the log pane, published by the renderer so page-scrolling and
    /// clamping match what is actually on screen.
    pub log_viewport: usize,
    /// Ticks observed, used for the "detecting" spinner and as a liveness hint.
    pub ticks: u64,
    /// External commands. Owned here so every view shares one drain point.
    pub processes: ProcessManager,
    pub devices: DeviceState,
    /// Created the first time the file browser is opened.
    pub browser: Option<Browser>,
    should_quit: bool,
}

impl App {
    pub fn new(start_dir: impl Into<PathBuf>) -> Self {
        Self {
            manager: ProjectManager::new(start_dir),
            logs: LogStore::default(),
            view: View::Dashboard,
            focus: Focus::Project,
            overlay: None,
            capability_cursor: 0,
            log_viewport: 1,
            ticks: 0,
            processes: ProcessManager::new(),
            devices: DeviceState::new(),
            browser: None,
            should_quit: false,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Runs the first detection and reports it to the log pane.
    ///
    /// A detection failure is surfaced, not fatal: the UI still starts so the
    /// user can read the error and override the backend.
    pub fn bootstrap(&mut self) {
        self.logs.info(format!(
            "working directory {}",
            self.manager.start_dir().display()
        ));
        self.detect();
    }

    /// Re-runs detection from the starting directory.
    pub fn detect(&mut self) {
        match self.manager.detect() {
            Ok(detection) => {
                let root = detection.root.display().to_string();
                let searched = detection.searched.len();
                let outcome = detection.outcome.clone();
                let source = detection.source;
                let confidence = detection.confidence();

                match &outcome {
                    DetectionOutcome::Detected(kind) => {
                        let confidence = confidence.unwrap_or(0.0);
                        if source == DetectionSource::Manual {
                            self.logs
                                .success(format!("{kind} selected manually at {root}"));
                        } else {
                            self.logs.success(format!(
                                "{kind} detected at {root} (confidence {confidence:.2})"
                            ));
                        }
                    }
                    DetectionOutcome::Ambiguous(kinds) => {
                        let names = kinds
                            .iter()
                            .map(|kind| kind.display_name())
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.logs.warn(format!(
                            "ambiguous project at {root}: {names} --- press 'o' to choose a backend"
                        ));
                    }
                    DetectionOutcome::Unknown => {
                        self.logs.warn(format!(
                            "no known project found in {searched} director{} from {root} --- press 'o' to select a backend",
                            if searched == 1 { "y" } else { "ies" }
                        ));
                    }
                }

                self.report_tools();
            }
            Err(err) => self.logs.error(err.to_string()),
        }
        self.clamp_capability_cursor();
    }

    /// Warns about required tools that are missing from `PATH`.
    fn report_tools(&mut self) {
        let Some(kind) = self.manager.selected_kind() else {
            return;
        };
        let missing: Vec<&str> = self
            .manager
            .registry()
            .tool_status(kind)
            .into_iter()
            .filter(|(_, available)| !*available)
            .map(|(tool, _)| tool)
            .collect();

        if !missing.is_empty() {
            self.logs.warn(format!(
                "{kind}: {} not found on PATH --- install it to enable the related operations",
                missing.join(", ")
            ));
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.on_key(key),
            // Ratatui re-renders from scratch each frame, so a resize only has
            // to invalidate what depends on the old geometry.
            AppEvent::Resize { .. } => self.logs.scroll_to_bottom(),
            AppEvent::Tick => self.ticks = self.ticks.wrapping_add(1),
            AppEvent::Process(event) => self.on_process(&event),
        }
    }

    /// Routes a process result to whatever asked for it.
    fn on_process(&mut self, event: &crate::process::ProcessEvent) {
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let update = browser.on_process(event, &mut self.processes, port.as_deref());
        self.browser = Some(browser);

        for (level, message) in update.notices {
            self.logs.push(level, message);
        }
        match update.device_scan {
            Some(Ok(devices)) => {
                let empty = devices.is_empty();
                self.devices.set_devices(devices);

                if empty {
                    self.set_device_pane_error(
                        "no MicroPython device found — connect a board and press 'd'",
                    );
                } else if self.devices.needs_selection() {
                    // Several boards: ask before touching any of them.
                    self.open_device_picker();
                } else {
                    // Exactly one, or a previous choice still present.
                    self.load_device_root();
                }
            }
            Some(Err(error)) => {
                self.devices.set_failed(error.clone());
                self.set_device_pane_error(error);
            }
            None => {}
        }
    }

    fn set_device_pane_error(&mut self, message: impl Into<String>) {
        if let Some(browser) = &mut self.browser {
            browser.set_device_error(message);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Raw mode swallows SIGINT, so Ctrl+C has to be handled explicitly and
        // must work regardless of focus or overlay.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit();
            return;
        }

        if self.overlay.is_some() {
            self.on_overlay_key(key);
            return;
        }

        match self.view {
            View::Dashboard => self.on_dashboard_key(key),
            View::Files => self.on_files_key(key),
        }
    }

    fn on_dashboard_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit(),
            KeyCode::Tab => self.focus = self.focus.step(true),
            KeyCode::BackTab => self.focus = self.focus.step(false),
            KeyCode::Char('r') => {
                self.logs.info("re-running project detection");
                self.detect();
            }
            KeyCode::Char('o') => self.open_picker(),
            KeyCode::Char('f') | KeyCode::F(2) => self.open_files(),
            KeyCode::Char('?') | KeyCode::F(1) => self.overlay = Some(Overlay::Help),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.page() as isize)),
            KeyCode::PageDown => self.move_cursor(self.page() as isize),
            KeyCode::Home => self.jump_to_start(),
            KeyCode::End => self.jump_to_end(),
            _ => {}
        }
    }

    fn on_files_key(&mut self, key: KeyEvent) {
        // `q`/esc leave the browser rather than the application: losing a
        // listing that took seconds to fetch by reflex would be hostile.
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.view = View::Dashboard;
                return;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.overlay = Some(Overlay::Help);
                return;
            }
            KeyCode::Char('d') => {
                self.scan_devices();
                return;
            }
            _ => {}
        }

        let Some(mut browser) = self.browser.take() else {
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let port = port.as_deref();

        let notices = match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                browser.toggle_focus();
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                browser.move_cursor(-1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                browser.move_cursor(1);
                Vec::new()
            }
            KeyCode::PageUp => {
                browser.move_cursor(-10);
                Vec::new()
            }
            KeyCode::PageDown => {
                browser.move_cursor(10);
                Vec::new()
            }
            KeyCode::Home => {
                browser.cursor_to(0);
                Vec::new()
            }
            KeyCode::End => {
                browser.cursor_to(usize::MAX);
                Vec::new()
            }
            KeyCode::Enter | KeyCode::Right => browser.enter(&mut self.processes, port),
            KeyCode::Backspace | KeyCode::Left => browser.ascend(&mut self.processes, port),
            KeyCode::Char('r') => {
                if browser.focus == Side::Device {
                    browser.load_device(&mut self.processes, port, true)
                } else {
                    browser.reload_local();
                    Vec::new()
                }
            }
            KeyCode::Char('h') => {
                browser.toggle_hidden();
                Vec::new()
            }
            KeyCode::Char('c') => browser.verify_selected(&mut self.processes, port),
            _ => Vec::new(),
        };

        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    /// Opens the file browser, if the selected backend has a filesystem.
    ///
    /// The gate is the capability, never the backend kind (`AGENTS.md` §3): any
    /// backend that declares [`Capability::Filesystem`] gets this view.
    pub fn open_files(&mut self) {
        if !self.manager.capabilities().contains(Capability::Filesystem) {
            let backend = self
                .manager
                .selected_kind()
                .map_or("no backend".to_string(), |kind| kind.to_string());
            self.logs
                .warn(format!("{backend} does not expose a device filesystem"));
            return;
        }

        self.view = View::Files;
        if self.browser.is_none() {
            let root = self
                .manager
                .root()
                .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
            self.browser = Some(Browser::new(root));
            // Only scan here. The listing waits for the scan to name a port:
            // issuing it now would let mpremote auto-connect to whichever board
            // answers first, which is the guess `SPEC.md` §8 forbids.
            self.scan_devices();
        }
    }

    fn scan_devices(&mut self) {
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        self.devices.set_scanning();
        // The device pane is waiting on this too, so it shows progress rather
        // than an idle prompt.
        browser.set_device_loading();
        let notices = browser.scan_devices(&mut self.processes, None);
        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    fn load_device_root(&mut self) {
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let notices = browser.load_device(&mut self.processes, port.as_deref(), false);
        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    fn on_overlay_key(&mut self, key: KeyEvent) {
        let Some(overlay) = self.overlay.clone() else {
            return;
        };
        match overlay {
            Overlay::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q')
                ) {
                    self.overlay = None;
                }
            }
            Overlay::BackendPicker { selected } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                KeyCode::Up | KeyCode::Char('k') => {
                    let count = PickerOption::all().len();
                    self.overlay = Some(Overlay::BackendPicker {
                        selected: (selected + count - 1) % count,
                    });
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let count = PickerOption::all().len();
                    self.overlay = Some(Overlay::BackendPicker {
                        selected: (selected + 1) % count,
                    });
                }
                KeyCode::Enter => {
                    self.apply_picker(selected);
                    self.overlay = None;
                }
                _ => {}
            },
            Overlay::DevicePicker { selected } => {
                let count = self.devices.devices().len().max(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::DevicePicker {
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::DevicePicker {
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter => {
                        self.apply_device_picker(selected);
                        self.overlay = None;
                    }
                    _ => {}
                }
            }
        }
    }

    fn open_device_picker(&mut self) {
        self.overlay = Some(Overlay::DevicePicker {
            selected: self.devices.selected_index().unwrap_or(0),
        });
    }

    fn apply_device_picker(&mut self, selected: usize) {
        if !self.devices.select(selected) {
            return;
        }
        let Some(device) = self.devices.selected() else {
            return;
        };
        self.logs.info(format!("device set to {}", device.label()));

        // The cached listing belongs to the previous board.
        if let Some(mut browser) = self.browser.take() {
            let port = self.devices.selected_port().map(str::to_string);
            let notices = browser.load_device(&mut self.processes, port.as_deref(), true);
            self.browser = Some(browser);
            for (level, message) in notices {
                self.logs.push(level, message);
            }
        }
    }

    fn open_picker(&mut self) {
        let current = self.manager.override_kind();
        let selected = PickerOption::all()
            .iter()
            .position(|option| match (option, current) {
                (PickerOption::Automatic, None) => true,
                (PickerOption::Backend(kind), Some(active)) => *kind == active,
                _ => false,
            })
            .unwrap_or(0);
        self.overlay = Some(Overlay::BackendPicker { selected });
    }

    fn apply_picker(&mut self, selected: usize) {
        let Some(option) = PickerOption::all().get(selected).copied() else {
            return;
        };
        match option {
            PickerOption::Automatic => {
                self.manager.set_override(None);
                match self.manager.selected_kind() {
                    Some(kind) => self
                        .logs
                        .info(format!("override cleared; detection selects {kind}")),
                    None => self
                        .logs
                        .warn("override cleared; detection did not identify a backend"),
                }
            }
            PickerOption::Backend(kind) => {
                self.manager.set_override(Some(kind));
                self.logs.info(format!("backend overridden to {kind}"));
                self.report_tools();
            }
        }
        self.clamp_capability_cursor();
    }

    /// Page size for the focused pane.
    fn page(&self) -> usize {
        match self.focus {
            Focus::Logs => self.log_viewport.max(1),
            _ => 5,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            Focus::Logs => {
                // The log pane scrolls; up means "towards older entries".
                if delta < 0 {
                    self.logs.scroll_up(delta.unsigned_abs(), self.log_viewport);
                } else {
                    self.logs.scroll_down(delta as usize);
                }
            }
            Focus::Capabilities => {
                let len = crate::backend::Capability::ALL.len();
                let next = (self.capability_cursor as isize + delta).clamp(0, len as isize - 1);
                self.capability_cursor = next as usize;
            }
            Focus::Project => {}
        }
    }

    fn jump_to_start(&mut self) {
        match self.focus {
            Focus::Logs => self.logs.scroll_up(usize::MAX, self.log_viewport),
            Focus::Capabilities => self.capability_cursor = 0,
            Focus::Project => {}
        }
    }

    fn jump_to_end(&mut self) {
        match self.focus {
            Focus::Logs => self.logs.scroll_to_bottom(),
            Focus::Capabilities => {
                self.capability_cursor = crate::backend::Capability::ALL.len().saturating_sub(1);
            }
            Focus::Project => {}
        }
    }

    fn clamp_capability_cursor(&mut self) {
        let len = crate::backend::Capability::ALL.len();
        self.capability_cursor = self.capability_cursor.min(len.saturating_sub(1));
    }

    /// Keybindings for the current context, rendered in the footer.
    pub fn shortcuts(&self) -> Vec<(&'static str, &'static str)> {
        match self.overlay {
            Some(Overlay::Help) => vec![("esc", "close")],
            Some(Overlay::BackendPicker { .. } | Overlay::DevicePicker { .. }) => {
                vec![("↑/↓", "select"), ("enter", "apply"), ("esc", "cancel")]
            }
            None => match self.view {
                View::Files => vec![
                    ("tab", "pane"),
                    ("enter", "open"),
                    ("bksp", "up"),
                    ("c", "compare"),
                    ("r", "reload"),
                    ("h", "hidden"),
                    ("d", "device"),
                    ("q", "back"),
                ],
                View::Dashboard => {
                    let mut keys = vec![("tab", "focus"), ("r", "re-detect"), ("o", "backend")];
                    if self.manager.capabilities().contains(Capability::Filesystem) {
                        keys.push(("f", "files"));
                    }
                    match self.focus {
                        Focus::Logs => keys.push(("↑/↓", "scroll")),
                        Focus::Capabilities => keys.push(("↑/↓", "move")),
                        Focus::Project => {}
                    }
                    keys.push(("?", "help"));
                    keys.push(("q", "quit"));
                    keys
                }
            },
        }
    }
}

/// Convenience for the binary: build an app rooted at the current directory.
pub fn app_from_cwd() -> Result<App> {
    Ok(App::new(std::env::current_dir()?))
}

/// Tick rate used by the binary. Re-exported here so the loop reads in one place.
pub const TICK_RATE: Duration = crate::event::DEFAULT_TICK_RATE;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Capability;

    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn app() -> App {
        App::new("/nonexistent-project-dir")
    }

    #[test]
    fn ctrl_c_quits_from_any_context() {
        for overlay in [
            None,
            Some(Overlay::Help),
            Some(Overlay::BackendPicker { selected: 0 }),
        ] {
            let mut app = app();
            app.overlay = overlay;
            app.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )));
            assert!(app.should_quit());
        }
    }

    #[test]
    fn esc_closes_the_overlay_instead_of_quitting() {
        let mut app = app();
        app.overlay = Some(Overlay::Help);
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.overlay, None);
        assert!(!app.should_quit());

        // With no overlay, esc leaves the application.
        app.handle(key(KeyCode::Esc));
        assert!(app.should_quit());
    }

    #[test]
    fn tab_cycles_focus_in_both_directions() {
        let mut app = app();
        assert_eq!(app.focus, Focus::Project);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Capabilities);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Logs);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Project);
        app.handle(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Logs);
    }

    #[test]
    fn picker_applies_and_clears_the_override() {
        let mut app = app();
        app.handle(key(KeyCode::Char('o')));
        assert_eq!(app.overlay, Some(Overlay::BackendPicker { selected: 0 }));

        // Move to the first real backend and apply it.
        app.handle(key(KeyCode::Down));
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.overlay, None);
        assert_eq!(app.manager.override_kind(), Some(BackendKind::MicroPython));

        // Re-opening starts on the active override, and Automatic clears it.
        app.handle(key(KeyCode::Char('o')));
        assert_eq!(app.overlay, Some(Overlay::BackendPicker { selected: 1 }));
        app.handle(key(KeyCode::Up));
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.manager.override_kind(), None);
    }

    #[test]
    fn picker_selection_wraps() {
        let mut app = app();
        app.handle(key(KeyCode::Char('o')));
        app.handle(key(KeyCode::Up));
        let last = PickerOption::all().len() - 1;
        assert_eq!(app.overlay, Some(Overlay::BackendPicker { selected: last }));
        app.handle(key(KeyCode::Down));
        assert_eq!(app.overlay, Some(Overlay::BackendPicker { selected: 0 }));
    }

    #[test]
    fn navigation_keys_apply_to_the_focused_pane_only() {
        let mut app = app();
        app.focus = Focus::Capabilities;
        app.handle(key(KeyCode::Down));
        assert_eq!(app.capability_cursor, 1);

        app.focus = Focus::Logs;
        app.handle(key(KeyCode::Down));
        assert_eq!(
            app.capability_cursor, 1,
            "log scrolling must not move the capability cursor"
        );
    }

    #[test]
    fn capability_cursor_stays_inside_the_list() {
        let mut app = app();
        app.focus = Focus::Capabilities;
        app.handle(key(KeyCode::End));
        assert_eq!(app.capability_cursor, Capability::ALL.len() - 1);
        app.handle(key(KeyCode::Down));
        assert_eq!(app.capability_cursor, Capability::ALL.len() - 1);
        app.handle(key(KeyCode::Home));
        assert_eq!(app.capability_cursor, 0);
        app.handle(key(KeyCode::Up));
        assert_eq!(app.capability_cursor, 0);
    }

    #[test]
    fn log_scrolling_respects_the_reported_viewport() {
        let mut app = app();
        app.focus = Focus::Logs;
        app.log_viewport = 2;
        for i in 0..10 {
            app.logs.info(format!("line {i}"));
        }

        app.handle(key(KeyCode::PageUp));
        assert_eq!(app.logs.scroll(), 2, "one page is one viewport height");
        app.handle(key(KeyCode::End));
        assert!(app.logs.is_following());
    }

    #[test]
    fn resize_re_pins_the_log_view_to_the_tail() {
        let mut app = app();
        app.log_viewport = 2;
        for i in 0..10 {
            app.logs.info(format!("line {i}"));
        }
        app.logs.scroll_up(3, 2);
        app.handle(AppEvent::Resize {
            width: 80,
            height: 24,
        });
        assert!(app.logs.is_following());
    }

    #[test]
    fn shortcuts_are_contextual() {
        let mut app = app();
        assert!(app.shortcuts().iter().any(|(key, _)| *key == "q"));

        app.overlay = Some(Overlay::BackendPicker { selected: 0 });
        let keys: Vec<&str> = app.shortcuts().iter().map(|(key, _)| *key).collect();
        assert!(keys.contains(&"enter"));
        assert!(
            !keys.contains(&"tab"),
            "pane keys are inert while a modal is open"
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut app = app();
        app.handle(key(KeyCode::Char('z')));
        assert!(!app.should_quit());
        assert_eq!(app.focus, Focus::Project);
    }
}
