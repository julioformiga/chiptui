//! Application state and event handling.
//!
//! The loop is deliberately thin: [`App::handle`] maps an [`AppEvent`] to a
//! state change and returns; rendering is a pure function of the state
//! afterwards. Nothing here blocks, so adding long-running work later means
//! adding events, not restructuring this file.
//!
//! This file holds `App`'s data (fields, [`Overlay`] and friends) and the
//! handful of methods that coordinate across subsystems (`handle`,
//! `on_process`, `on_key`, `shortcuts`). Everything that belongs to one
//! subsystem instead lives in a submodule so no single file tracks every
//! concern at once: [`devices`] (scanning/picking a device or backend),
//! [`file_browser`] (the local/device file panes and viewer),
//! [`flash_view`] (the `esptool` menu and online firmware search), and
//! [`overlay`] (modal key dispatch, including the shared confirm-dialog
//! machinery). They are `impl App` blocks split by file, not separate
//! types --- cross-submodule calls use `pub(super)`, so nothing outside
//! `app` gains access that the flat file did not already expose.

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::backend::{BackendKind, Capability};
use crate::browser::{Browser, Side};
use crate::device::{DevicePath, DeviceState};
use crate::error::Result;
use crate::event::AppEvent;
use crate::flash::{FlashPanel, FlashScreen};
use crate::logs::LogStore;
use crate::process::ProcessManager;
use crate::project::{DetectionOutcome, DetectionSource, ProjectManager};

pub mod devices;
pub mod file_browser;
pub mod flash_view;
pub mod overlay;

/// Which screen is showing.
///
/// The file browser lives inside the Dashboard's second row rather than as
/// its own screen (`SPEC.md` §11): it needs no separate view to leave, so
/// there is no `View::Files` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    /// `esptool` chip/flash information, erase and write, kept apart from the
    /// filesystem browser (`SPEC.md` §9).
    Flash,
}

/// Which pane receives navigation keys.
///
/// `FilesLocal`/`FilesDevice` are the dashboard's two file-browser columns;
/// each is its own stop so `Tab` walks all four dashboard columns in one
/// consistent tour instead of a separate sub-focus inside the files row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Project,
    FilesLocal,
    FilesDevice,
    Logs,
}

/// Which tab row 3 is showing. `Left`/`Right` switch between them while
/// [`Focus::Logs`] holds focus --- unbound otherwise, so the two-column file
/// browser's own `Left`/`Right` handling never collides with this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogTab {
    #[default]
    Log,
    Monitor,
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
    /// A destructive esptool action awaiting explicit confirmation
    /// (`SPEC.md` §15). `message` is the literal command about to run, never
    /// a paraphrase.
    Confirm {
        message: String,
        confirm: bool,
    },
    /// Firmware file selection when more than one `.bin`/`.elf` was found in
    /// `firmware/`.
    FirmwarePicker {
        selected: usize,
    },
    /// Empty or unrecognized project: asks which backend this directory is
    /// (`SPEC.md` §7). Unlike [`Overlay::BackendPicker`] this fires
    /// automatically, offers no "Automatic" row (detection already failed to
    /// conclude one), and persists the choice to `chiptui.toml`.
    ProjectSetup {
        selected: usize,
    },
    /// A firmware download would overwrite a file already in `firmware/`;
    /// needs explicit confirmation before running (`SPEC.md` §15 applied to
    /// a filesystem write rather than a device operation).
    ConfirmDownloadOverwrite {
        url: String,
        dest: PathBuf,
        confirm: bool,
    },
    /// Ask for confirmation before uploading a file.
    ConfirmUpload {
        name: String,
        confirm: bool,
    },
    /// The text file under the cursor in the files pane (`enter`): a small
    /// menu of what to do with it. Which three actions show up depends on
    /// which pane --- see [`FileAction::for_side`].
    FileActions {
        side: Side,
        name: String,
        selected: usize,
    },
    /// A file's contents, opened by choosing `View` from [`Overlay::FileActions`]
    /// (`SPEC.md` §2's secondary goal to support external editors, not build
    /// one). Holds no data itself --- [`App::viewer`] does, so scrolling never
    /// re-clones the file the way rebuilding an `Overlay` variant on every
    /// key press would.
    FileViewer,
    /// A device file was edited and just finished re-uploading: offer a
    /// restart so the change actually takes effect, with a btop-style
    /// Yes/No button pair. `confirm` is which one is highlighted --- starts
    /// on `false` (No), unlike every other confirm overlay here, since
    /// restarting interrupts whatever the board is currently doing and
    /// should never happen from a reflex `Enter`.
    ConfirmRestartDevice {
        confirm: bool,
    },
    /// Ask to flash MicroPython if device is unresponsive.
    ConfirmEraseForMicroPython {
        confirm: bool,
    },
    /// Ask for confirmation before deleting a file.
    ConfirmDelete {
        side: Side,
        name: String,
        confirm: bool,
    },
}

/// One action offered by [`Overlay::FileActions`] for the file under the
/// cursor. The files pane is a sync tool, not a filesystem manager, so the
/// choices mirror that: move a copy across, or work with the copy already
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    SendToDevice,
    Download,
    View,
    Edit,
    Delete,
}

impl FileAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::SendToDevice => "📤 Send to device",
            Self::Download => "📥 Download",
            Self::View => "👁  View",
            Self::Edit => "📝 Edit",
            Self::Delete => "🗑  Delete",
        }
    }

    /// The three actions offered for a file in `side`, in menu order.
    pub fn for_side(side: Side) -> &'static [FileAction] {
        match side {
            Side::Local => &[Self::SendToDevice, Self::View, Self::Edit, Self::Delete],
            Side::Device => &[Self::Download, Self::View, Self::Edit, Self::Delete],
        }
    }
}

/// Contents behind [`Overlay::FileViewer`].
pub struct FileViewer {
    pub source: ViewerSource,
    pub state: ViewerState,
    /// Index of the first visible line, clamped in [`App::scroll_viewer`]
    /// against the viewport height the renderer publishes each frame
    /// (mirrors [`App::log_viewport`]).
    pub scroll: usize,
}

impl FileViewer {
    /// Name to detect a syntax-highlighting language from --- the file name
    /// alone either way, since a device file has no local path to draw one
    /// from.
    pub fn display_name(&self) -> String {
        match &self.source {
            ViewerSource::Local(path) => path.display().to_string(),
            ViewerSource::Device(path) => path.to_string(),
        }
    }
}

/// Where a viewer's content came from. A local read is synchronous
/// ([`App::open_local_file_viewer`] fills in [`ViewerState`] immediately); a
/// device `cat` is not, so a device-sourced viewer starts in
/// [`ViewerState::Loading`] and is updated once [`crate::browser::DeviceView`]
/// arrives (`App::apply_device_view`, matched by path so a stale reply for a
/// viewer the user already closed and reopened elsewhere is dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerSource {
    Local(PathBuf),
    Device(DevicePath),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerState {
    Loading,
    Ready {
        lines: Vec<String>,
    },
    /// The file could not be shown (binary, too large, unreadable, or the
    /// device `cat` failed) --- the reason is already in the log too.
    Error(String),
}

/// A file queued for `$EDITOR`, and where to send it back to afterward.
/// Built by [`App::take_pending_edit`]'s callers (`main.rs`'s event loop),
/// which owns the terminal handle needed to actually suspend and run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdit {
    pub path: PathBuf,
    /// Set when this edit came from a device file (`FileAction::Edit` on the
    /// device pane, or the viewer's `e` on a device-sourced viewer): a
    /// successful `$EDITOR` exit re-uploads here
    /// ([`App::request_device_reupload`]) instead of just reloading the
    /// local pane.
    pub device_target: Option<DevicePath>,
}

/// A pending interactive monitor session to be run by the event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMonitor {
    pub command: crate::process::Command,
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
    /// Which of row 3's tabs is showing.
    pub log_tab: LogTab,
    /// Which live feed the Monitor tab renders.
    pub monitor_source: MonitorSource,
    pub overlay: Option<Overlay>,
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
    /// Created the first time the flash view is opened.
    pub flash: Option<FlashPanel>,
    /// Backs [`Overlay::FileViewer`] while it is open.
    pub viewer: Option<FileViewer>,
    /// Height of the file viewer, published by the renderer each frame so
    /// paging matches what is actually on screen (mirrors [`Self::log_viewport`]).
    pub viewer_viewport: usize,
    /// Set by the viewer's `e` key; consumed by the binary's event loop,
    /// which owns the terminal handle needed to suspend the alternate screen
    /// for `$EDITOR`. `App` cannot run it itself --- it has no access to
    /// [`crate::terminal::TerminalGuard`].
    pending_edit: Option<PendingEdit>,

    /// The interactive device monitor session spawned inside a PTY.
    pub device_monitor_process: Option<crate::process::ProcessId>,
    /// Accumulated lines from the PTY session.
    pub device_monitor_output: Vec<String>,
    pending_monitor: Option<PendingMonitor>,
    /// Set by [`App::defer_device_info_query`] when a device is newly
    /// selected while `mpremote` is (or is about to be) busy; consumed the
    /// moment [`Self::browser`] next goes idle. `esptool` and `mpremote`
    /// hold the serial port exclusively, so the background chip/flash query
    /// must never race the file listing that a fresh device selection also
    /// kicks off (`AGENTS.md` §5's "one tool at a time" applies across
    /// tools too, not just within `mpremote`).
    flash_query_pending: bool,
    should_quit: bool,
    last_port_count: Option<usize>,
}

impl App {
    pub fn new(start_dir: impl Into<PathBuf>) -> Self {
        Self {
            manager: ProjectManager::new(start_dir),
            logs: LogStore::default(),
            view: View::Dashboard,
            focus: Focus::Project,
            log_tab: LogTab::default(),
            monitor_source: MonitorSource::default(),
            overlay: None,
            log_viewport: 1,
            ticks: 0,
            processes: ProcessManager::new(),
            devices: DeviceState::new(),
            browser: None,
            flash: None,
            viewer: None,
            viewer_viewport: 1,
            pending_edit: None,
            device_monitor_process: None,
            device_monitor_output: Vec::new(),
            pending_monitor: None,
            flash_query_pending: false,
            should_quit: false,
            last_port_count: None,
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
        self.clamp_focus();
    }

    /// Opens [`Overlay::ProjectSetup`] when detection has nothing to go on:
    /// `Unknown` or `Ambiguous`, with no session override and no scaffold
    /// file already deciding it (`SPEC.md` §7's empty-project prompt).
    ///
    /// Deliberately **not** called from inside [`App::detect`] itself.
    /// `detect()`/`bootstrap()` are called directly by many existing tests
    /// that assert on `overlay`/send key events right afterwards (every
    /// `tests/flash_view.rs` case via its `app_with_flash` helper, which
    /// starts from a bare temp directory). Auto-opening a modal from inside
    /// `detect()` would silently redirect their next key press into
    /// `on_overlay_key`. Instead only the two real "detection just ran and
    /// the user might act on it" call sites opt in explicitly: the binary's
    /// startup sequence, and the `r` re-detect key.
    pub fn maybe_open_project_setup(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        let Some(detection) = self.manager.detection() else {
            return;
        };
        if self.manager.override_kind().is_some() || detection.source == DetectionSource::Config {
            return;
        }
        if matches!(
            detection.outcome,
            DetectionOutcome::Unknown | DetectionOutcome::Ambiguous(_)
        ) {
            self.overlay = Some(Overlay::ProjectSetup { selected: 0 });
        }
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
            AppEvent::Tick => {
                self.ticks = self.ticks.wrapping_add(1);
                self.check_device_hotplug();
            }
            AppEvent::Process(event) => self.on_process(&event),
        }
    }

    fn check_device_hotplug(&mut self) {
        if !self.manager.capabilities().contains(Capability::Filesystem) {
            return;
        }
        // only check every 4 ticks (1 second)
        if !self.ticks.is_multiple_of(4) {
            return;
        }

        if matches!(
            self.devices.discovery,
            crate::device::DiscoveryState::Scanning
        ) {
            return;
        }
        if self.browser.as_ref().is_some_and(Browser::is_busy) {
            return;
        }
        if self.device_monitor_process.is_some() {
            return;
        }

        let current_count = crate::device::count_serial_ports();
        if let Some(current) = current_count {
            if let Some(last) = self.last_port_count
                && current != last
            {
                self.logs
                    .info("device connection change detected, rescanning...");
                self.scan_devices();
            }
            self.last_port_count = Some(current);
        }
    }

    /// Routes a process result to whatever asked for it.
    ///
    /// Both subsystems see every event: each guards on its own in-flight
    /// process id and is a no-op for an event it did not start, which is
    /// simpler than tracking ownership here.
    fn on_process(&mut self, event: &crate::process::ProcessEvent) {
        match event {
            crate::process::ProcessEvent::Line {
                id,
                stream: _,
                text,
            } if Some(*id) == self.device_monitor_process => {
                self.device_monitor_output.push(text.clone());
                return;
            }
            crate::process::ProcessEvent::Output { id, text }
                if Some(*id) == self.device_monitor_process =>
            {
                if self.device_monitor_output.is_empty() {
                    self.device_monitor_output.push(String::new());
                }
                for char in text.chars() {
                    match char {
                        '\n' => self.device_monitor_output.push(String::new()),
                        '\r' => {}
                        '\x08' | '\x7f' => {
                            if let Some(last) = self.device_monitor_output.last_mut() {
                                last.pop();
                            }
                        }
                        _ => {
                            if let Some(last) = self.device_monitor_output.last_mut() {
                                last.push(char);
                            }
                        }
                    }
                }
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome,
                duration: _,
            } if Some(*id) == self.device_monitor_process => {
                self.device_monitor_process = None;
                self.device_monitor_output
                    .push(format!("\r\n[monitor {}]", outcome.summary()));
                return;
            }
            _ => {}
        }

        if let Some(mut browser) = self.browser.take() {
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
                        self.defer_device_info_query();
                    }
                }
                Some(Err(error)) => {
                    self.devices.set_failed(error.clone());
                    self.set_device_pane_error(error);
                }
                None => {}
            }
            if let Some(view) = update.device_view {
                self.apply_device_view(view);
            }
            if update.prompt_micropython_flash
                && !matches!(
                    self.overlay,
                    Some(Overlay::ConfirmEraseForMicroPython { .. })
                )
            {
                self.overlay = Some(Overlay::ConfirmEraseForMicroPython { confirm: false });
            }
            if let Some(transfer) = update.transfer {
                self.apply_transfer(transfer);
            }
        }

        if let Some(mut flash) = self.flash.take() {
            let update = flash.on_process(event);
            let fetch_update = flash.on_curl_process(event);
            self.flash = Some(flash);

            for (level, message) in update.notices {
                self.logs.push(level, message);
            }
            if update.offer_flash {
                self.offer_flash_after_erase();
            }
            if update.search_online_for_firmware {
                self.search_online();
                self.view = View::Flash;
            }

            for (level, message) in fetch_update.notices {
                self.logs.push(level, message);
            }
            if fetch_update.download_finished {
                self.offer_flash_after_download();
            }
        }

        // The deferred query (above, and from `apply_device_picker`) can only
        // start once `mpremote` has released the port --- checked last, after
        // both blocks above have had a chance to move `self.browser` back to
        // idle for this same event.
        if self.flash_query_pending && !self.browser.as_ref().is_some_and(Browser::is_busy) {
            self.flash_query_pending = false;
            self.maybe_query_device_info();
        }
    }

    fn set_device_pane_error(&mut self, message: impl Into<String>) {
        if let Some(browser) = &mut self.browser {
            browser.set_device_error(message);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let is_monitor_active = self.focus == Focus::Logs
            && self.log_tab == LogTab::Monitor
            && self.monitor_source == MonitorSource::Device
            && self.device_monitor_process.is_some();

        if is_monitor_active {
            if let Some((id, bytes)) = self.device_monitor_process.zip(key_to_bytes(key)) {
                self.processes.write_stdin(id, &bytes);
            }
            return;
        }

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
            View::Flash => self.on_flash_key(key),
        }
    }

    /// Focus order for `Tab`/`BackTab`: the two file-browser columns are only
    /// stops when the backend actually has something to list there
    /// (`AGENTS.md` §3 --- gated on the capability, not the backend kind).
    fn focus_order(&self) -> Vec<Focus> {
        let mut order = vec![Focus::Project];
        if self.manager.capabilities().contains(Capability::Filesystem) {
            order.push(Focus::FilesLocal);
            order.push(Focus::FilesDevice);
        }
        order.push(Focus::Logs);
        order
    }

    fn step_focus(&mut self, forward: bool) {
        let order = self.focus_order();
        let len = order.len();
        let index = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        self.focus = order[next];
    }

    /// Pulls focus back onto `Project` when it was sitting on a files column
    /// that just lost its capability (a backend switch away from
    /// MicroPython) --- otherwise it would point at a pane that no longer
    /// exists in the layout.
    fn clamp_focus(&mut self) {
        if matches!(self.focus, Focus::FilesLocal | Focus::FilesDevice)
            && !self.manager.capabilities().contains(Capability::Filesystem)
        {
            self.focus = Focus::Project;
        }
    }

    fn on_dashboard_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.quit();
                return;
            }
            KeyCode::Tab => {
                self.step_focus(true);
                return;
            }
            KeyCode::BackTab => {
                self.step_focus(false);
                return;
            }
            KeyCode::Char('o') => {
                self.open_picker();
                return;
            }
            KeyCode::Char('x') => {
                self.open_flash();
                return;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.overlay = Some(Overlay::Help);
                return;
            }
            KeyCode::Char('d') if self.manager.capabilities().contains(Capability::Filesystem) => {
                self.scan_devices();
                return;
            }
            KeyCode::Char('m') if self.manager.capabilities().contains(Capability::Monitor) => {
                self.open_monitor();
                return;
            }
            _ => {}
        }

        if matches!(self.focus, Focus::FilesLocal | Focus::FilesDevice) {
            self.on_files_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('r') => {
                self.logs.info("re-running project detection");
                self.detect();
                self.maybe_open_project_setup();
            }
            KeyCode::Left if self.focus == Focus::Logs => self.log_tab = LogTab::Log,
            KeyCode::Right if self.focus == Focus::Logs => {
                if self.manager.capabilities().contains(Capability::Monitor) {
                    self.log_tab = LogTab::Monitor;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.page() as isize)),
            KeyCode::PageDown => self.move_cursor(self.page() as isize),
            KeyCode::Home => self.jump_to_start(),
            KeyCode::End => self.jump_to_end(),
            _ => {}
        }
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
            // The Monitor tab always tails its live output; only Log scrolls.
            Focus::Logs if self.log_tab == LogTab::Log => {
                // The log pane scrolls; up means "towards older entries".
                if delta < 0 {
                    self.logs.scroll_up(delta.unsigned_abs(), self.log_viewport);
                } else {
                    self.logs.scroll_down(delta as usize);
                }
            }
            // FilesLocal/FilesDevice never reach here: on_dashboard_key routes
            // them to on_files_key first.
            Focus::Project | Focus::FilesLocal | Focus::FilesDevice | Focus::Logs => {}
        }
    }

    fn jump_to_start(&mut self) {
        match self.focus {
            Focus::Logs if self.log_tab == LogTab::Log => {
                self.logs.scroll_up(usize::MAX, self.log_viewport);
            }
            Focus::Project | Focus::FilesLocal | Focus::FilesDevice | Focus::Logs => {}
        }
    }

    fn jump_to_end(&mut self) {
        match self.focus {
            Focus::Logs if self.log_tab == LogTab::Log => self.logs.scroll_to_bottom(),
            Focus::Project | Focus::FilesLocal | Focus::FilesDevice | Focus::Logs => {}
        }
    }

    /// Keybindings for the current context, rendered in the footer.
    pub fn shortcuts(&self) -> Vec<(&'static str, &'static str)> {
        match self.overlay {
            Some(Overlay::Help) => vec![("esc", "close")],
            Some(Overlay::FileViewer) => vec![
                ("↑/↓", "scroll"),
                ("pgup/pgdn", "page"),
                ("e", "edit with $EDITOR"),
                ("q/esc", "close"),
            ],
            Some(
                Overlay::BackendPicker { .. }
                | Overlay::DevicePicker { .. }
                | Overlay::FirmwarePicker { .. }
                | Overlay::ProjectSetup { .. }
                | Overlay::FileActions { .. },
            ) => {
                vec![("↑/↓", "select"), ("enter", "apply"), ("esc", "cancel")]
            }
            Some(
                Overlay::Confirm { .. }
                | Overlay::ConfirmDownloadOverwrite { .. }
                | Overlay::ConfirmDelete { .. }
                | Overlay::ConfirmUpload { .. }
                | Overlay::ConfirmRestartDevice { .. }
                | Overlay::ConfirmEraseForMicroPython { .. },
            ) => {
                vec![
                    ("←/→", "choose"),
                    ("enter", "confirm"),
                    ("y/n", "quick reply"),
                    ("esc", "cancel"),
                ]
            }
            None => match self.view {
                View::Flash => match self.flash.as_ref().map(|flash| flash.screen) {
                    Some(FlashScreen::Options) => vec![
                        ("tab", "field"),
                        ("←/→", "cycle"),
                        ("type", "edit"),
                        ("enter", "run"),
                        ("q", "menu"),
                    ],
                    Some(FlashScreen::Menu) => vec![
                        ("↑/↓", "select"),
                        ("enter", "run"),
                        ("s", "search online"),
                        ("u", "paste URL"),
                        ("q", "back"),
                    ],
                    Some(FlashScreen::OnlineBoards | FlashScreen::OnlineFirmware) => {
                        vec![("↑/↓", "select"), ("enter", "choose"), ("q", "menu")]
                    }
                    Some(FlashScreen::CustomUrl) => {
                        vec![("type", "edit"), ("enter", "download"), ("q", "menu")]
                    }
                    None => vec![("↑/↓", "select"), ("enter", "run"), ("q", "back")],
                },
                View::Dashboard => {
                    let mut keys = vec![("tab", "focus")];
                    if matches!(self.focus, Focus::FilesLocal | Focus::FilesDevice) {
                        keys.push(("enter", "open"));
                        keys.push(("bksp", "up"));
                        keys.push(("r", "reload"));
                        keys.push(("c", "compare"));
                        keys.push(("h", "hidden"));
                    } else {
                        keys.push(("r", "re-detect"));
                    }
                    keys.push(("o", "backend"));
                    let caps = self.manager.capabilities();
                    if caps.contains(Capability::Flash) || caps.contains(Capability::EraseFlash) {
                        keys.push(("x", "flash"));
                    }
                    if self.focus == Focus::Logs {
                        if caps.contains(Capability::Monitor) {
                            keys.push(("←/→", "log/monitor"));
                        }
                        if self.log_tab == LogTab::Log {
                            keys.push(("↑/↓", "scroll"));
                        }
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

fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let b = c.to_ascii_uppercase() as u8 ^ 0x40;
            Some(vec![b])
        }
        KeyCode::Char(c) => {
            let mut b = [0; 4];
            Some(c.encode_utf8(&mut b).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7F]), // DEL
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            app.focus,
            Focus::Logs,
            "no filesystem capability yet, so Tab skips the files panes"
        );
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Project);
        app.handle(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Logs);
    }

    #[test]
    fn tab_visits_the_files_panes_when_the_backend_has_a_filesystem() {
        // Needs a real directory: `set_override` only transforms an existing
        // `Detection`, and detecting against a nonexistent path never
        // produces one.
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::MicroPython));
        assert_eq!(app.focus, Focus::Project);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::FilesLocal);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::FilesDevice);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Logs);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Project);
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

    #[test]
    fn log_tab_defaults_to_log() {
        assert_eq!(app().log_tab, LogTab::Log);
    }

    #[test]
    fn left_right_switch_the_log_tab_only_while_logs_is_focused() {
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::MicroPython));
        app.focus = Focus::Logs;

        app.handle(key(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Monitor);
        app.handle(key(KeyCode::Left));
        assert_eq!(app.log_tab, LogTab::Log);

        // Elsewhere, Left/Right must not touch it (e.g. reserved for other
        // panes' own navigation).
        app.focus = Focus::Project;
        app.handle(key(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Log);
    }

    #[test]
    fn scrolling_is_inert_on_the_monitor_tab() {
        let mut app = app();
        app.focus = Focus::Logs;
        app.log_tab = LogTab::Monitor;
        app.logs.info("a line");

        app.handle(key(KeyCode::Up));
        assert!(
            app.logs.is_following(),
            "the log store must not scroll while the Monitor tab is showing"
        );
    }

    #[test]
    fn shortcuts_mention_the_monitor_tab_only_when_the_capability_exists() {
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::MicroPython));
        app.focus = Focus::Logs;

        let keys: Vec<&str> = app.shortcuts().iter().map(|(key, _)| *key).collect();
        assert!(
            keys.contains(&"←/→"),
            "MicroPython declares Capability::Monitor"
        );

        app.log_tab = LogTab::Monitor;
        let keys: Vec<&str> = app.shortcuts().iter().map(|(key, _)| *key).collect();
        assert!(
            !keys.contains(&"↑/↓"),
            "the scroll hint belongs to the Log tab only"
        );
    }
}
