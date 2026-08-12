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
use crate::browser::{Browser, DeviceView, Notice, Side, Transfer, TransferKind};
use crate::device::{DevicePath, DeviceState};
use crate::error::Result;
use crate::event::AppEvent;
use crate::files;
use crate::flash::{FlashAction, FlashPanel, FlashScreen, OptionsField};
use crate::logs::LogStore;
use crate::process::ProcessManager;
use crate::project::{DetectionOutcome, DetectionSource, ProjectManager};

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

    /// An erase just succeeded and firmware discovery already ran
    /// (`FlashPanel::on_process`). Never flashes on its own --- that still
    /// needs its own confirmation --- only decides whether to ask which file
    /// or go straight to the options screen for the one that was found.
    ///
    /// The erase itself already moved the user to the Monitor tab
    /// (`show_flash_in_monitor`), so when there is something to offer, this
    /// reopens the Flash dialog --- the write-flash step needs the user
    /// looking at it, not left on a tab they may not be watching.
    fn offer_flash_after_erase(&mut self) {
        let Some(flash) = &mut self.flash else {
            return;
        };
        flash.set_cursor_to(FlashAction::WriteFlash);
        match flash.firmware.len() {
            0 => {}
            1 => {
                flash.screen = FlashScreen::Options;
                self.view = View::Flash;
            }
            _ => {
                self.overlay = Some(Overlay::FirmwarePicker { selected: 0 });
                self.view = View::Flash;
            }
        }
    }

    /// A firmware file just finished downloading. Picks it up as an ordinary
    /// local candidate and hands off to the exact same confirmed
    /// `erase_flash` → `write_flash` chain the Flash menu already uses
    /// (`SPEC.md` §9) --- its confirm overlay, showing the literal command,
    /// *is* "ask the user whether to erase_flash and flash"; no separate
    /// combined action or confirmation type is needed.
    fn offer_flash_after_download(&mut self) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let notices = flash.discover_firmware();
        flash.screen = FlashScreen::Menu;
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
        self.trigger_flash_action(FlashAction::EraseFlash);
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

    /// Handles a key while [`Focus::FilesLocal`]/[`Focus::FilesDevice`] holds
    /// focus. `Tab`/`BackTab`, `o`, `x`, `?` and `d` are dashboard-wide and
    /// already handled by [`App::on_dashboard_key`] before this is reached,
    /// so only the file browser's own navigation remains here.
    fn on_files_key(&mut self, key: KeyEvent) {
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        // The two columns are separate `Focus` stops now, so the browser's
        // own notion of which side is active just follows it.
        browser.focus = match self.focus {
            Focus::FilesDevice => Side::Device,
            _ => Side::Local,
        };
        let port = self.devices.selected_port().map(str::to_string);
        let port = port.as_deref();

        let notices = match key.code {
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
            KeyCode::Enter | KeyCode::Right => match browser.selected_actionable_name() {
                Some(name) => {
                    self.overlay = Some(Overlay::FileActions {
                        side: browser.focus,
                        name,
                        selected: 0,
                    });
                    Vec::new()
                }
                None => browser.enter(&mut self.processes, port),
            },
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

    /// Runs the action chosen from [`Overlay::FileActions`]. `name` is the
    /// file's name in whichever directory `side` currently shows --- stable
    /// for the duration of the menu, since an open overlay routes every key
    /// to [`App::on_overlay_key`] instead of the browser's own navigation.
    fn run_file_action(&mut self, side: Side, name: &str, action: FileAction) {
        match (side, action) {
            (Side::Local, FileAction::View) => {
                let Some(browser) = &self.browser else { return };
                self.open_local_file_viewer(browser.local_path.join(name));
            }
            (Side::Local, FileAction::Edit) => {
                let Some(browser) = &self.browser else { return };
                self.pending_edit = Some(PendingEdit {
                    path: browser.local_path.join(name),
                    device_target: None,
                });
            }
            (Side::Local, FileAction::SendToDevice) => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_upload(name, processes, port)
                });
            }
            (Side::Local, FileAction::Delete) => {
                self.overlay = Some(Overlay::ConfirmDelete {
                    side: Side::Local,
                    name: name.to_string(),
                    confirm: false,
                });
            }
            (Side::Device, FileAction::View) => self.open_device_file_viewer(name),
            (Side::Device, FileAction::Download) => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_download(name, processes, port)
                });
            }
            (Side::Device, FileAction::Edit) => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_edit_download(name, processes, port)
                });
            }
            (Side::Device, FileAction::Delete) => {
                self.overlay = Some(Overlay::ConfirmDelete {
                    side: Side::Device,
                    name: name.to_string(),
                    confirm: false,
                });
            }
            // `FileAction::for_side` never offers `Download` on `Local` or
            // `SendToDevice` on `Device`.
            (Side::Local, FileAction::Download) | (Side::Device, FileAction::SendToDevice) => {}
        }
    }

    fn delete_file(&mut self, side: Side, name: &str) {
        match side {
            Side::Local => {
                if let Some(browser) = &mut self.browser {
                    let path = browser.local_path.join(name);
                    match std::fs::remove_file(&path) {
                        Ok(_) => {
                            self.logs.success(format!("{} removed", path.display()));
                            browser.reload_local();
                        }
                        Err(e) => {
                            self.logs.error(format!("{}: remove failed: {e}", path.display()));
                        }
                    }
                }
            }
            Side::Device => {
                self.dispatch_browser(|browser, processes, port| {
                    browser.request_remove_device(name, processes, port)
                });
            }
        }
    }

    /// Takes `self.browser` for the duration of `f`, supplying the selected
    /// port, then puts it back and logs whatever `f` reports --- the same
    /// take/replace/log shape every browser-mutating key handler here
    /// already repeats, pulled out for the three new file-transfer actions.
    fn dispatch_browser(
        &mut self,
        f: impl FnOnce(&mut Browser, &mut ProcessManager, Option<&str>) -> Vec<Notice>,
    ) {
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let notices = f(&mut browser, &mut self.processes, port.as_deref());
        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    /// Reads `path` and opens [`Overlay::FileViewer`] over it, synchronously
    /// --- a local read never has to wait. A file that cannot be shown
    /// (binary, too large, unreadable) still opens the viewer, with the
    /// reason in place of content, rather than doing nothing.
    fn open_local_file_viewer(&mut self, path: PathBuf) {
        let state = match files::read_text_file(&path) {
            Ok(content) => ViewerState::Ready {
                lines: content.lines().map(str::to_string).collect(),
            },
            Err(message) => ViewerState::Error(message),
        };
        self.viewer = Some(FileViewer {
            source: ViewerSource::Local(path),
            state,
            scroll: 0,
        });
        self.overlay = Some(Overlay::FileViewer);
    }

    /// Opens [`Overlay::FileViewer`] on `name`, in the current device
    /// directory, and queues the `cat` that will fill it in --- unlike the
    /// local case this cannot be synchronous, so the viewer opens straight
    /// into [`ViewerState::Loading`].
    fn open_device_file_viewer(&mut self, name: &str) {
        let Some(browser) = &self.browser else { return };
        let path = browser.device_path.join(name);

        if let Some(size) = browser.device_entry_size(name)
            && size > files::MAX_VIEW_BYTES
        {
            self.logs.warn(format!(
                "{path}: too large to preview ({} MiB) --- use 'Download' or 'Edit' instead",
                size / (1024 * 1024)
            ));
            return;
        }

        self.viewer = Some(FileViewer {
            source: ViewerSource::Device(path),
            state: ViewerState::Loading,
            scroll: 0,
        });
        self.overlay = Some(Overlay::FileViewer);
        self.dispatch_browser(|browser, processes, port| {
            browser.request_device_view(name, processes, port)
        });
    }

    /// Feeds a finished device `cat` into the open viewer, if it is still the
    /// one waiting on it --- matched by path, so a reply for a viewer the
    /// user already closed (or replaced by opening a different file) is
    /// dropped instead of overwriting the wrong content.
    fn apply_device_view(&mut self, view: DeviceView) {
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        let ViewerSource::Device(path) = &viewer.source else {
            return;
        };
        if *path != view.path {
            return;
        }
        viewer.state = match view.content {
            Ok(content) => ViewerState::Ready {
                lines: content.lines().map(str::to_string).collect(),
            },
            Err(message) => ViewerState::Error(message),
        };
    }

    /// Reacts to a finished download or upload. A download queued by
    /// [`FileAction::Edit`] on a device file lands locally here --- queue
    /// `$EDITOR` on it. The re-upload that follows once `$EDITOR` closes
    /// (`App::request_device_reupload`) offers a restart on success. A plain
    /// download and an ordinary "Send to device" upload are fire-and-forget:
    /// their outcome is already in the log via `notices`, nothing further to
    /// do here.
    fn apply_transfer(&mut self, transfer: Transfer) {
        let Transfer { kind, ok } = transfer;
        match kind {
            TransferKind::Download {
                local_path,
                then_edit: true,
                source,
            } if ok => {
                self.pending_edit = Some(PendingEdit {
                    path: local_path,
                    device_target: Some(source),
                });
            }
            TransferKind::Upload { after_edit: true } if ok => {
                self.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });
            }
            _ => {}
        }
    }

    /// Re-uploads `local_path` to `target` after `$EDITOR` closes
    /// successfully on a device file --- called by the binary, which is the
    /// only place that knows the editor actually ran and exited cleanly.
    pub fn request_device_reupload(&mut self, local_path: PathBuf, target: DevicePath) {
        self.dispatch_browser(|browser, processes, port| {
            browser.request_reupload_after_edit(local_path, target, processes, port)
        });
    }

    /// Restarts the device (`soft-reset`), once the user has explicitly
    /// confirmed it from [`Overlay::ConfirmRestartDevice`].
    fn restart_device(&mut self) {
        self.dispatch_browser(|browser, processes, port| browser.request_reset(processes, port));
    }

    /// Scrolls the open file viewer, clamped to the last position that still
    /// keeps the viewport full --- same shape as [`LogStore::scroll_up`], just
    /// counting down from the top instead of up from the tail. A no-op while
    /// [`ViewerState::Loading`] or [`ViewerState::Error`]: there is nothing to
    /// page through yet.
    fn scroll_viewer(&mut self, delta: isize) {
        let viewport = self.viewer_viewport.max(1);
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        let ViewerState::Ready { lines } = &viewer.state else {
            return;
        };
        let max = lines.len().saturating_sub(viewport) as isize;
        let next = (viewer.scroll as isize + delta).clamp(0, max.max(0));
        viewer.scroll = next as usize;
    }

    /// Jumps the open file viewer straight to `target`, clamped the same way
    /// as [`Self::scroll_viewer`]. `usize::MAX` means "the end", mirroring
    /// [`Browser::cursor_to`]'s convention for the same key (`End`).
    fn jump_viewer(&mut self, target: usize) {
        let viewport = self.viewer_viewport.max(1);
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        let ViewerState::Ready { lines } = &viewer.state else {
            return;
        };
        let max = lines.len().saturating_sub(viewport);
        viewer.scroll = target.min(max);
    }

    /// Takes the path queued by an `Edit` action, if any. The binary's event
    /// loop polls this once per iteration and, when it fires, suspends the
    /// terminal to run `$EDITOR` --- `App` has no terminal handle to do that
    /// itself.
    pub fn take_pending_edit(&mut self) -> Option<PendingEdit> {
        self.pending_edit.take()
    }

    pub fn take_pending_monitor(&mut self) -> Option<PendingMonitor> {
        self.pending_monitor.take()
    }

    /// Re-reads the local pane after `$EDITOR` closes: size and contents may
    /// have changed under it while the terminal was suspended.
    pub fn reload_local_files(&mut self) {
        if let Some(browser) = &mut self.browser {
            browser.reload_local();
        }
    }

    /// `q`/esc step back one screen (Options/Output to Menu) rather than
    /// leaving straight to the dashboard, mirroring the file browser's "do
    /// not throw work away by reflex" rule --- except from the top-level
    /// menu, where there is nowhere closer to go.
    fn on_flash_key(&mut self, key: KeyEvent) {
        let Some(screen) = self.flash.as_ref().map(|flash| flash.screen) else {
            return;
        };

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                match screen {
                    FlashScreen::Menu => self.view = View::Dashboard,
                    FlashScreen::Options
                    | FlashScreen::OnlineBoards
                    | FlashScreen::OnlineFirmware
                    | FlashScreen::CustomUrl => {
                        if let Some(flash) = &mut self.flash {
                            flash.screen = FlashScreen::Menu;
                        }
                    }
                }
                return;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.overlay = Some(Overlay::Help);
                return;
            }
            _ => {}
        }

        match screen {
            FlashScreen::Menu => self.on_flash_menu_key(key),
            FlashScreen::Options => self.on_flash_options_key(key),
            FlashScreen::OnlineBoards => self.on_flash_online_boards_key(key),
            FlashScreen::OnlineFirmware => self.on_flash_online_firmware_key(key),
            FlashScreen::CustomUrl => self.on_flash_custom_url_key(key),
        }
    }

    fn on_flash_menu_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => flash.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => flash.move_cursor(1),
            KeyCode::Char('s') => {
                self.flash = Some(flash);
                self.search_online();
                return;
            }
            KeyCode::Char('u') => {
                flash.custom_url.clear();
                flash.screen = FlashScreen::CustomUrl;
            }
            KeyCode::Enter => {
                let action = flash.selected_action();
                self.flash = Some(flash);
                self.trigger_flash_action(action);
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    /// Searches micropython.org/download/ for the currently known chip
    /// (`SPEC.md` §9), narrowed by the selected device's board vendor when
    /// its vid:pid identifies one (`DeviceInfo::board_vendor`).
    fn search_online(&mut self) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let Some(mcu) = flash
            .chip
            .family()
            .map(|family| family.micropython_mcu_filter())
        else {
            self.flash = Some(flash);
            self.logs
                .warn("no chip known yet --- run 'chip information' or pick one in Options first");
            return;
        };
        let vendor = self
            .devices
            .selected()
            .and_then(|device| device.board_vendor());
        let notices = flash.search_online(mcu, vendor, &mut self.processes);
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    fn on_flash_online_boards_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let count = flash.online_boards.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => flash.move_online_cursor(-1, count),
            KeyCode::Down | KeyCode::Char('j') => flash.move_online_cursor(1, count),
            KeyCode::Enter => {
                let Some(board_id) = flash
                    .online_boards
                    .get(flash.online_cursor)
                    .map(|board| board.id.clone())
                else {
                    self.flash = Some(flash);
                    return;
                };
                self.flash = Some(flash);
                self.fetch_selected_board(&board_id);
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    fn on_flash_online_firmware_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let count = flash.online_firmware.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => flash.move_online_cursor(-1, count),
            KeyCode::Down | KeyCode::Char('j') => flash.move_online_cursor(1, count),
            KeyCode::Enter => {
                let url = flash
                    .online_firmware
                    .get(flash.online_cursor)
                    .map(|file| file.url.clone());
                self.flash = Some(flash);
                if let Some(url) = url {
                    self.request_download(url);
                }
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    fn on_flash_custom_url_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        match key.code {
            KeyCode::Backspace => flash.backspace_custom_url(),
            KeyCode::Char(c) => flash.push_custom_url_char(c),
            KeyCode::Enter => {
                let url = flash.custom_url.trim().to_string();
                self.flash = Some(flash);
                if url.is_empty() {
                    self.logs.warn("type or paste a URL first");
                } else {
                    self.request_download(url);
                }
                return;
            }
            _ => {}
        }
        self.flash = Some(flash);
    }

    fn fetch_selected_board(&mut self, board_id: &str) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let notices = flash.fetch_board_page(board_id, &mut self.processes);
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    /// Starts a download, first asking for confirmation if it would
    /// overwrite a file already in `firmware/` (`SPEC.md` §15).
    fn request_download(&mut self, url: String) {
        let Some(flash) = &self.flash else {
            return;
        };
        let Some(dest) = flash.download_destination(&url) else {
            self.logs.warn("cannot determine a filename from that URL");
            return;
        };
        if dest.exists() {
            self.overlay = Some(Overlay::ConfirmDownloadOverwrite { url, dest });
        } else {
            self.start_download(url, dest);
        }
    }

    fn start_download(&mut self, url: String, dest: PathBuf) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let notices = flash.download(&url, dest, &mut self.processes);
        self.flash = Some(flash);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    fn on_flash_options_key(&mut self, key: KeyEvent) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let action = flash.selected_action();

        match key.code {
            KeyCode::Tab => flash.step_options_focus(action, true),
            KeyCode::BackTab => flash.step_options_focus(action, false),
            KeyCode::Left => self.step_option_value(&mut flash, false),
            KeyCode::Right => self.step_option_value(&mut flash, true),
            KeyCode::Backspace => match flash.options_focus {
                OptionsField::Offset => flash.backspace_offset(),
                OptionsField::ExtraArgs => flash.backspace_extra_args(),
                _ => {}
            },
            KeyCode::Char(c) => match flash.options_focus {
                OptionsField::Offset => flash.push_offset_char(c),
                OptionsField::ExtraArgs => flash.push_extra_arg_char(c),
                _ => {}
            },
            KeyCode::Enter => {
                self.flash = Some(flash);
                self.trigger_flash_action(action);
                return;
            }
            _ => {}
        }

        self.flash = Some(flash);
    }

    /// `Left`/`Right` only do something on a cyclable field --- text fields
    /// (offset, custom flags) are edited by typing, not arrowing through.
    fn step_option_value(&self, flash: &mut FlashPanel, forward: bool) {
        match flash.options_focus {
            OptionsField::Chip => flash.cycle_chip(forward),
            OptionsField::FlashMode => flash.cycle_flash_mode(forward),
            OptionsField::FlashFreq => flash.cycle_flash_freq(forward),
            OptionsField::FlashSize => flash.cycle_flash_size(forward),
            OptionsField::Offset | OptionsField::ExtraArgs => {}
        }
    }

    /// Starts `action`, or gathers what it needs first: a firmware file
    /// (`FlashAction::needs_firmware`) or explicit confirmation
    /// (`FlashAction::is_destructive`, `SPEC.md` §15). Shared by the menu and
    /// the options screen, and by the automatic post-erase offer.
    fn trigger_flash_action(&mut self, action: FlashAction) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };

        if action.needs_firmware() && flash.selected_firmware.is_none() {
            let notices = flash.discover_firmware();
            for (level, message) in notices {
                self.logs.push(level, message);
            }
            match flash.firmware.len() {
                0 => {}
                1 => {
                    flash.screen = FlashScreen::Options;
                    flash.options_focus = OptionsField::Chip;
                }
                _ => self.overlay = Some(Overlay::FirmwarePicker { selected: 0 }),
            }
            self.flash = Some(flash);
            return;
        }

        if let Some(reason) = flash.blocked_reason(action) {
            self.logs.warn(reason);
            self.flash = Some(flash);
            return;
        }

        let port = self.devices.selected_port().map(str::to_string);
        if action.is_destructive() {
            flash.request_confirmation(action);
            let message = flash
                .command_preview(action, port.as_deref())
                .unwrap_or_else(|| "(command unavailable)".to_string());
            self.overlay = Some(Overlay::Confirm { message });
            self.flash = Some(flash);
        } else {
            let notices = flash.run(action, &mut self.processes, port.as_deref());
            self.flash = Some(flash);
            if notices.is_empty() {
                self.show_flash_in_monitor();
            }
            for (level, message) in notices {
                self.logs.push(level, message);
            }
        }
    }

    /// Closes the Flash dialog and moves focus to the Monitor tab, which
    /// streams [`FlashPanel::output`] as it arrives --- called after a flash
    /// action successfully spawns, replacing the dialog's former
    /// `FlashScreen::Output` screen (`SPEC.md` §11).
    fn show_flash_in_monitor(&mut self) {
        self.view = View::Dashboard;
        self.focus = Focus::Logs;
        self.log_tab = LogTab::Monitor;
        self.monitor_source = MonitorSource::Flash;
    }

    /// Starts an interactive device monitor.
    pub fn open_monitor(&mut self) {
        let port = self.devices.selected_port().map(str::to_string);
        if let Some(command) = self
            .manager
            .backend()
            .and_then(|b| b.monitor_command(port.as_deref()))
        {
            self.log_tab = LogTab::Monitor;
            self.monitor_source = MonitorSource::Device;
            self.device_monitor_output.clear();

            // Spawn the monitor in a PTY so it stays inside the tab
            match self
                .processes
                .spawn_pty(command, std::time::Duration::from_secs(86400))
            {
                // 24h timeout
                Ok(id) => self.device_monitor_process = Some(id),
                Err(e) => {
                    self.logs.error(format!("could not open monitor: {}", e));
                    self.device_monitor_process = None;
                }
            }
        }
    }

    /// Opens the flash view, if the selected backend can flash or erase.
    ///
    /// The gate is the capability, never the backend kind (`AGENTS.md` §3).
    pub fn open_flash(&mut self) {
        if !self.ensure_flash_panel() {
            let backend = self
                .manager
                .selected_kind()
                .map_or("no backend".to_string(), |kind| kind.to_string());
            self.logs
                .warn(format!("{backend} does not expose flash operations"));
            return;
        }
        self.view = View::Flash;
    }

    /// Capability-gates and lazily creates [`Self::flash`] --- the shared
    /// prerequisite between opening the Flash view by hand and
    /// [`App::maybe_query_device_info`]'s background refresh. `false` means
    /// the backend has neither `Flash` nor `EraseFlash`.
    fn ensure_flash_panel(&mut self) -> bool {
        let caps = self.manager.capabilities();
        if !caps.contains(Capability::Flash) && !caps.contains(Capability::EraseFlash) {
            return false;
        }
        if self.flash.is_none() {
            let root = self
                .manager
                .root()
                .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
            let firmware_dir = root.join("firmware");
            // Lazily created here rather than only via `ensure_micropython_layout`
            // so opening Flash still works for a project that never went
            // through the empty-project prompt (e.g. an existing MicroPython
            // project detected automatically, `SPEC.md` §7).
            if let Err(err) = std::fs::create_dir_all(&firmware_dir) {
                self.logs.warn(format!(
                    "could not create {}: {err}",
                    firmware_dir.display()
                ));
            }
            self.flash = Some(FlashPanel::new(firmware_dir));
        }
        true
    }

    /// Marks a background chip/flash query as due, without starting it yet
    /// --- `esptool` cannot open the serial port while `mpremote` (just
    /// kicked off by `load_device_root`, right before every call site of
    /// this method) still holds it exclusively. `on_process` starts the
    /// query for real once [`Self::browser`] reports idle again.
    fn defer_device_info_query(&mut self) {
        self.flash_query_pending = true;
    }

    /// Kicks off a background `flash-id` so the Dashboard's device panel has
    /// something to show without the user ever opening the Flash view ---
    /// same shape as [`App::maybe_scan_devices`]'s "load this eagerly, once
    /// the prerequisite is known", just for esptool instead of mpremote.
    /// Only ever called once [`Self::defer_device_info_query`]'s wait for
    /// `mpremote` to release the port is satisfied. A no-op when the backend
    /// has no flash capability or a flash command is already running;
    /// [`FlashPanel::query_device_info`]'s synchronous rejection notice is
    /// swallowed on purpose --- a courtesy refresh finding the panel busy is
    /// not something the user needs to hear about, unlike its eventual
    /// success or failure, which still reaches the log the normal way once
    /// the process finishes.
    fn maybe_query_device_info(&mut self) {
        if !self.ensure_flash_panel() {
            return;
        }
        let Some(port) = self.devices.selected_port().map(str::to_string) else {
            return;
        };
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        flash.query_device_info(&mut self.processes, Some(&port));
        self.flash = Some(flash);
    }

    /// Scans for a connected device as soon as the project is known, without
    /// waiting for the user to move focus onto a file browser pane. A no-op
    /// once a browser already exists (`AGENTS.md` §5's "one `mpremote` at a
    /// time" applies just as much to not re-issuing a scan that already ran).
    ///
    /// Called from three places: `main.rs` right after startup, and
    /// [`App::apply_project_setup`]/[`App::apply_picker`] --- any moment the
    /// selected backend could newly gain [`Capability::Filesystem`].
    /// Deliberately **not** called from [`App::bootstrap`] itself, for the
    /// same reason [`App::maybe_open_project_setup`] is not: many existing
    /// tests call `bootstrap()` directly and do not expect a subprocess to
    /// spawn as a side effect.
    pub fn maybe_scan_devices(&mut self) {
        if !self.manager.capabilities().contains(Capability::Filesystem) {
            return;
        }
        self.ensure_browser_scanning();
    }

    /// Creates the browser and starts a device scan, if neither has already
    /// happened. Only the listing itself waits for the scan to name a port:
    /// issuing the scan now would let mpremote auto-connect to whichever
    /// board answers first, which is the guess `SPEC.md` §8 forbids.
    fn ensure_browser_scanning(&mut self) {
        if self.browser.is_some() {
            return;
        }
        let root = self
            .manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
        self.browser = Some(Browser::new(Self::initial_local_dir(root)));
        self.scan_devices();
    }

    /// Files opens on `src/` when the project has one --- the directory kept
    /// in sync with the device (`SPEC.md` §9) --- so the local pane starts
    /// showing exactly what a `Filesystem` upload would send. Falls back to
    /// the project root for projects without a `src/` (Zephyr, or a
    /// MicroPython project that predates this layout).
    fn initial_local_dir(root: PathBuf) -> PathBuf {
        let src = root.join("src");
        if src.is_dir() { src } else { root }
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
            Overlay::Confirm { .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.overlay = None;
                    self.confirm_flash_action();
                }
                KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                    self.overlay = None;
                    if let Some(flash) = &mut self.flash {
                        flash.cancel_pending();
                    }
                }
                _ => {}
            },
            Overlay::FirmwarePicker { selected } => {
                let count = self
                    .flash
                    .as_ref()
                    .map_or(0, |flash| flash.firmware.len())
                    .max(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::FirmwarePicker {
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::FirmwarePicker {
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter => {
                        self.apply_firmware_picker(selected);
                        self.overlay = None;
                    }
                    _ => {}
                }
            }
            Overlay::ProjectSetup { selected } => {
                let count = BackendKind::ALL.len();
                match key.code {
                    // No `q`/esc-cancels-quietly here: leaving this open
                    // means the project stays unrecognized, which is exactly
                    // what re-running detection will ask about again.
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::ProjectSetup {
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::ProjectSetup {
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter => {
                        self.apply_project_setup(selected);
                        self.overlay = None;
                    }
                    _ => {}
                }
            }
            Overlay::ConfirmDownloadOverwrite { url, dest } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.overlay = None;
                    self.start_download(url, dest);
                }
                KeyCode::Esc | KeyCode::Char('n' | 'q') => self.overlay = None,
                _ => {}
            },
            Overlay::FileActions {
                side,
                name,
                selected,
            } => {
                let count = FileAction::for_side(side).len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Some(Overlay::FileActions {
                            side,
                            name,
                            selected: (selected + count - 1) % count,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.overlay = Some(Overlay::FileActions {
                            side,
                            name,
                            selected: (selected + 1) % count,
                        });
                    }
                    KeyCode::Enter => {
                        let action = FileAction::for_side(side)[selected];
                        self.overlay = None;
                        self.run_file_action(side, &name, action);
                    }
                    _ => {}
                }
            }
            Overlay::FileViewer => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.overlay = None;
                    self.viewer = None;
                }
                KeyCode::Char('e') => {
                    let source = self.viewer.as_ref().map(|viewer| viewer.source.clone());
                    self.overlay = None;
                    self.viewer = None;
                    match source {
                        Some(ViewerSource::Local(path)) => {
                            self.pending_edit = Some(PendingEdit {
                                path,
                                device_target: None,
                            });
                        }
                        Some(ViewerSource::Device(path)) => {
                            let name = path.name().to_string();
                            self.dispatch_browser(|browser, processes, port| {
                                browser.request_edit_download(&name, processes, port)
                            });
                        }
                        None => {}
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => self.scroll_viewer(-1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll_viewer(1),
                KeyCode::PageUp => self.scroll_viewer(-(self.viewer_viewport.max(1) as isize)),
                KeyCode::PageDown => self.scroll_viewer(self.viewer_viewport.max(1) as isize),
                KeyCode::Home => self.jump_viewer(0),
                KeyCode::End => self.jump_viewer(usize::MAX),
                _ => {}
            },
            // Default *no*: `confirm` starts `false` (No highlighted), so a
            // reflex `Enter` dismisses instead of restarting, unlike every
            // other confirm overlay here --- a restart interrupts whatever
            // the board is doing. Left/Right (and h/l) move the highlight
            // between the two buttons; `y`/`n` still jump straight to an
            // answer for muscle memory.
            Overlay::ConfirmRestartDevice { confirm } => match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Char('h' | 'l') => {
                    self.overlay = Some(Overlay::ConfirmRestartDevice { confirm: !confirm });
                }
                KeyCode::Char('y') => {
                    self.overlay = None;
                    self.restart_device();
                }
                KeyCode::Char('n') => self.overlay = None,
                KeyCode::Enter => {
                    self.overlay = None;
                    if confirm {
                        self.restart_device();
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                _ => {}
            },
            Overlay::ConfirmDelete {
                side,
                ref name,
                confirm,
            } => match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Char('h' | 'l') => {
                    self.overlay = Some(Overlay::ConfirmDelete {
                        side,
                        name: name.clone(),
                        confirm: !confirm,
                    });
                }
                KeyCode::Char('y') => {
                    let side = side;
                    let name = name.clone();
                    self.overlay = None;
                    self.delete_file(side, &name);
                }
                KeyCode::Char('n') => self.overlay = None,
                KeyCode::Enter => {
                    let side = side;
                    let name = name.clone();
                    let do_it = confirm;
                    self.overlay = None;
                    if do_it {
                        self.delete_file(side, &name);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                _ => {}
            },
        }
    }

    /// Runs the action a [`Overlay::Confirm`] was guarding, once the user
    /// accepted it.
    fn confirm_flash_action(&mut self) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let Some(action) = flash.take_pending() else {
            self.flash = Some(flash);
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let notices = flash.run(action, &mut self.processes, port.as_deref());
        self.flash = Some(flash);
        if notices.is_empty() {
            self.show_flash_in_monitor();
        }
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    fn apply_firmware_picker(&mut self, selected: usize) {
        let Some(flash) = &mut self.flash else {
            return;
        };
        if !flash.select_firmware(selected) {
            return;
        }
        flash.screen = FlashScreen::Options;
        flash.options_focus = OptionsField::Chip;
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
        match self.browser.take() {
            Some(mut browser) => {
                let port = self.devices.selected_port().map(str::to_string);
                let notices = browser.load_device(&mut self.processes, port.as_deref(), true);
                self.browser = Some(browser);
                for (level, message) in notices {
                    self.logs.push(level, message);
                }
                // Same port-contention reasoning as the startup path: wait
                // for this listing to finish before esptool touches the port.
                self.defer_device_info_query();
            }
            // No filesystem capability, so nothing is about to contend for
            // the port --- safe to query right away.
            None => self.maybe_query_device_info(),
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
        // A switch away from a backend with no filesystem (e.g. Zephyr) never
        // created a browser to scan with; a switch onto one should not still
        // be sitting on "not scanned" just because it happened via the
        // picker instead of at startup.
        self.maybe_scan_devices();
        self.clamp_focus();
    }

    /// Applies the empty-project prompt's answer: an in-session override
    /// exactly like [`App::apply_picker`], plus persisting the choice to
    /// `chiptui.toml` so the directory needs no prompt on later runs.
    fn apply_project_setup(&mut self, selected: usize) {
        let Some(kind) = BackendKind::ALL.get(selected).copied() else {
            return;
        };
        self.manager.set_override(Some(kind));
        match self.manager.write_scaffold(kind) {
            Ok(()) => self.logs.success(format!(
                "{kind} selected and saved to chiptui.toml in this project"
            )),
            Err(err) => self.logs.warn(format!(
                "{kind} selected for this session, but chiptui.toml could not be saved: {err}"
            )),
        }
        if kind == BackendKind::MicroPython
            && let Err(err) = self.manager.ensure_micropython_layout()
        {
            self.logs
                .warn(format!("could not create src/firmware layout: {err}"));
        }
        self.report_tools();
        self.maybe_scan_devices();
        self.clamp_focus();
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
            Some(Overlay::Confirm { .. } | Overlay::ConfirmDownloadOverwrite { .. } | Overlay::ConfirmDelete { .. }) => {
                vec![("y/enter", "confirm"), ("n/esc", "cancel")]
            }
            Some(Overlay::ConfirmRestartDevice { .. }) => {
                vec![
                    ("←/→", "choose"),
                    ("enter", "confirm"),
                    ("y/n", "restart/skip"),
                    ("esc", "skip"),
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
