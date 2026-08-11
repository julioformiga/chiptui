//! Dual-pane file browser state.
//!
//! Holds both panes, the device listing cache and the in-flight `mpremote`
//! requests. It never touches the log or the terminal: results come back as
//! [`Notice`] values the caller forwards, which keeps the whole state machine
//! testable without a UI.
//!
//! Device commands are serialised --- `mpremote` opens the serial port
//! exclusively, so two concurrent listings would fight over it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::micropython::{commands, parse};
use crate::device::DevicePath;
use crate::files::{self, LocalEntry, SyncStatus, Verdicts};
use crate::logs::Level;
use crate::process::{Outcome, ProcessEvent, ProcessId, ProcessManager, Stream};

/// Device commands are quick, but a board in a bad state can hang the port.
pub const DEVICE_TIMEOUT: Duration = Duration::from_secs(20);

/// Local files above this are not hashed, to keep the UI thread responsive.
const MAX_LOCAL_HASH_BYTES: u64 = 32 * 1024 * 1024;

/// A message for the log pane.
pub type Notice = (Level, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Device,
}

impl Side {
    pub const fn other(self) -> Self {
        match self {
            Self::Local => Self::Device,
            Self::Device => Self::Local,
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Local => "Local",
            Self::Device => "Device",
        }
    }
}

/// What the device pane currently holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneState {
    /// Nothing requested yet.
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// What a spawned process was for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    Devices,
    List(DevicePath),
    Hash {
        /// Entry name, used to record the verdict.
        name: String,
        /// The locally computed digest to compare against.
        local_digest: String,
    },
}

/// stdout/stderr collected for one in-flight process.
#[derive(Default)]
struct Output {
    stdout: String,
    stderr: String,
}

pub struct Browser {
    pub local_path: PathBuf,
    pub local_entries: Vec<LocalEntry>,
    pub local_error: Option<String>,
    pub local_cursor: usize,

    pub device_path: DevicePath,
    pub device_state: PaneState,
    pub device_cursor: usize,

    pub focus: Side,
    pub show_hidden: bool,

    /// Listings by device path: each `ls` costs seconds over serial.
    cache: BTreeMap<DevicePath, Vec<parse::RemoteEntry>>,
    verdicts: Verdicts,

    queue: VecDeque<Request>,
    in_flight: Option<(ProcessId, Request)>,
    output: HashMap<ProcessId, Output>,

    /// Overrides the `mpremote` executable. `None` means "resolve on PATH".
    tool_path: Option<String>,
}

impl Browser {
    pub fn new(local_path: impl Into<PathBuf>) -> Self {
        let mut browser = Self {
            local_path: local_path.into(),
            local_entries: Vec::new(),
            local_error: None,
            local_cursor: 0,
            device_path: DevicePath::root(),
            device_state: PaneState::Idle,
            device_cursor: 0,
            focus: Side::Local,
            show_hidden: false,
            cache: BTreeMap::new(),
            verdicts: Verdicts::new(),
            queue: VecDeque::new(),
            in_flight: None,
            output: HashMap::new(),
            tool_path: None,
        };
        browser.reload_local();
        browser
    }

    /// Points device commands at a specific `mpremote` binary.
    ///
    /// Used by tests to substitute a fake, and the seam `SPEC.md` §13's
    /// `[tools]` configuration will plug into.
    pub fn set_tool_path(&mut self, program: impl Into<String>) {
        self.tool_path = Some(program.into());
    }

    // ---- local pane -----------------------------------------------------

    pub fn reload_local(&mut self) {
        match files::read_dir(&self.local_path) {
            Ok(entries) => {
                self.local_entries = entries;
                self.local_error = None;
            }
            Err(source) => {
                self.local_entries.clear();
                self.local_error = Some(format!(
                    "cannot read {}: {source}",
                    self.local_path.display()
                ));
            }
        }
        self.clamp_cursors();
    }

    /// Entries shown in the local pane, honouring the hidden-files toggle.
    pub fn visible_local(&self) -> Vec<&LocalEntry> {
        self.local_entries
            .iter()
            .filter(|entry| self.show_hidden || !files::is_hidden(&entry.name))
            .collect()
    }

    /// Entries shown in the device pane.
    pub fn visible_device(&self) -> Vec<&parse::RemoteEntry> {
        self.cache
            .get(&self.device_path)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| self.show_hidden || !files::is_hidden(&entry.name))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Comparison of the two directories currently displayed.
    pub fn statuses(&self) -> BTreeMap<String, SyncStatus> {
        let local: Vec<LocalEntry> = self.visible_local().into_iter().cloned().collect();
        let device: Vec<parse::RemoteEntry> = self.visible_device().into_iter().cloned().collect();
        files::compare(&local, &device, &self.verdicts)
    }

    // ---- navigation -----------------------------------------------------

    pub fn cursor(&self, side: Side) -> usize {
        match side {
            Side::Local => self.local_cursor,
            Side::Device => self.device_cursor,
        }
    }

    pub fn len(&self, side: Side) -> usize {
        match side {
            Side::Local => self.visible_local().len(),
            Side::Device => self.visible_device().len(),
        }
    }

    pub fn is_empty(&self, side: Side) -> bool {
        self.len(side) == 0
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.len(self.focus);
        if len == 0 {
            return;
        }
        let current = self.cursor(self.focus) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        match self.focus {
            Side::Local => self.local_cursor = next,
            Side::Device => self.device_cursor = next,
        }
    }

    pub fn cursor_to(&mut self, index: usize) {
        let len = self.len(self.focus);
        let index = index.min(len.saturating_sub(1));
        match self.focus {
            Side::Local => self.local_cursor = index,
            Side::Device => self.device_cursor = index,
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = self.focus.other();
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.clamp_cursors();
    }

    /// Name under the cursor in `side`.
    pub fn selected_name(&self, side: Side) -> Option<String> {
        match side {
            Side::Local => self
                .visible_local()
                .get(self.local_cursor)
                .map(|entry| entry.name.clone()),
            Side::Device => self
                .visible_device()
                .get(self.device_cursor)
                .map(|entry| entry.name.clone()),
        }
    }

    fn selected_is_dir(&self, side: Side) -> bool {
        match side {
            Side::Local => self
                .visible_local()
                .get(self.local_cursor)
                .is_some_and(|entry| entry.is_dir),
            Side::Device => self
                .visible_device()
                .get(self.device_cursor)
                .is_some_and(|entry| entry.is_dir),
        }
    }

    /// Descends into the directory under the cursor.
    pub fn enter(&mut self, processes: &mut ProcessManager, port: Option<&str>) -> Vec<Notice> {
        if !self.selected_is_dir(self.focus) {
            return Vec::new();
        }
        let Some(name) = self.selected_name(self.focus) else {
            return Vec::new();
        };

        match self.focus {
            Side::Local => {
                self.local_path = self.local_path.join(name);
                self.local_cursor = 0;
                self.reload_local();
                Vec::new()
            }
            Side::Device => {
                self.device_path = self.device_path.join(&name);
                self.device_cursor = 0;
                self.load_device(processes, port, false)
            }
        }
    }

    /// Moves the focused pane to its parent directory.
    pub fn ascend(&mut self, processes: &mut ProcessManager, port: Option<&str>) -> Vec<Notice> {
        match self.focus {
            Side::Local => {
                if let Some(parent) = self.local_path.parent().map(Path::to_path_buf) {
                    self.local_path = parent;
                    self.local_cursor = 0;
                    self.reload_local();
                }
                Vec::new()
            }
            Side::Device => match self.device_path.parent() {
                Some(parent) => {
                    self.device_path = parent;
                    self.device_cursor = 0;
                    self.load_device(processes, port, false)
                }
                None => Vec::new(),
            },
        }
    }

    // ---- device requests ------------------------------------------------

    /// Loads the current device directory, reusing the cache unless `force`.
    pub fn load_device(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
        force: bool,
    ) -> Vec<Notice> {
        if force {
            self.cache.remove(&self.device_path);
            self.verdicts.clear();
        }
        if self.cache.contains_key(&self.device_path) {
            self.device_state = PaneState::Ready;
            self.clamp_cursors();
            return Vec::new();
        }

        self.device_state = PaneState::Loading;
        self.enqueue(Request::List(self.device_path.clone()), processes, port)
    }

    /// Enumerates serial devices.
    pub fn scan_devices(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        self.enqueue(Request::Devices, processes, port)
    }

    /// Puts the device pane into an error state.
    ///
    /// Used for failures that happen outside a listing --- no board attached,
    /// or a scan that could not run at all.
    pub fn set_device_error(&mut self, message: impl Into<String>) {
        self.device_state = PaneState::Failed(message.into());
    }

    /// Marks the device pane as waiting on a command.
    pub fn set_device_loading(&mut self) {
        self.device_state = PaneState::Loading;
    }

    /// Compares the selected file's contents on both sides.
    ///
    /// The local digest is computed here and the device digest is requested;
    /// the verdict is recorded when the process finishes.
    pub fn verify_selected(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let Some(name) = self.selected_name(self.focus) else {
            return vec![(Level::Warn, "nothing selected to compare".to_string())];
        };

        let status = self.statuses().get(&name).copied();
        match status {
            Some(status) if status.is_verifiable() => {}
            Some(status) => {
                return vec![(
                    Level::Warn,
                    format!("{name}: cannot compare contents — {}", status.describe()),
                )];
            }
            None => return Vec::new(),
        }

        let local_file = self.local_path.join(&name);
        let local_digest = match hash_local(&local_file) {
            Ok(digest) => digest,
            Err(error) => return vec![(Level::Error, format!("{name}: {error}"))],
        };

        let mut notices = vec![(Level::Info, format!("comparing {name} by sha256"))];
        notices.extend(self.enqueue(Request::Hash { name, local_digest }, processes, port));
        notices
    }

    /// Queues a request, starting it if the device is free.
    fn enqueue(
        &mut self,
        request: Request,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        self.queue.push_back(request);
        self.pump(processes, port)
    }

    /// Starts the next queued request if nothing is in flight.
    fn pump(&mut self, processes: &mut ProcessManager, port: Option<&str>) -> Vec<Notice> {
        if self.in_flight.is_some() {
            return Vec::new();
        }
        let Some(request) = self.queue.pop_front() else {
            return Vec::new();
        };

        let command = match &request {
            Request::Devices => commands::list_devices(),
            Request::List(path) => commands::list_dir(port, path),
            Request::Hash { name, .. } => commands::sha256(port, &self.device_path.join(name)),
        };
        let command = match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        };

        let id = processes.spawn(command, DEVICE_TIMEOUT);
        self.in_flight = Some((id, request));
        self.output.insert(id, Output::default());
        Vec::new()
    }

    /// Feeds a process event back into the browser.
    ///
    /// Returns messages for the log and, for device discovery, the parsed
    /// device list so the caller can update its device state.
    pub fn on_process(
        &mut self,
        event: &ProcessEvent,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> BrowserUpdate {
        let mut update = BrowserUpdate::default();

        match event {
            ProcessEvent::Started { .. } => return update,
            ProcessEvent::Line { id, stream, text } => {
                if let Some(output) = self.output.get_mut(id) {
                    let buffer = match stream {
                        Stream::Stdout => &mut output.stdout,
                        Stream::Stderr => &mut output.stderr,
                    };
                    buffer.push_str(text);
                    buffer.push('\n');
                }
                return update;
            }
            ProcessEvent::Finished { id, outcome, .. } => {
                let Some((in_flight, request)) = self.in_flight.take() else {
                    return update;
                };
                if in_flight != *id {
                    // Not ours (another subsystem's process): put it back.
                    self.in_flight = Some((in_flight, request));
                    return update;
                }

                let output = self.output.remove(id).unwrap_or_default();
                self.complete(&request, outcome, &output, &mut update);
                update.notices.extend(self.pump(processes, port));
            }
        }

        update
    }

    fn complete(
        &mut self,
        request: &Request,
        outcome: &Outcome,
        output: &Output,
        update: &mut BrowserUpdate,
    ) {
        let failure = match outcome {
            Outcome::Success => None,
            Outcome::SpawnFailed(_) => Some(format!(
                "{} is not on PATH — install it to browse device files",
                commands::PROGRAM
            )),
            Outcome::TimedOut => Some(format!(
                "{} did not respond within {}s — the board may be busy or unplugged",
                commands::PROGRAM,
                DEVICE_TIMEOUT.as_secs()
            )),
            Outcome::Cancelled => Some("cancelled".to_string()),
            Outcome::Failed { .. } => Some(parse::explain_error(&output.stderr)),
        };

        match request {
            Request::Devices => match failure {
                Some(error) => {
                    update.notices.push((Level::Error, error.clone()));
                    update.device_scan = Some(Err(error));
                }
                None => {
                    let devices = parse::parse_devices(&output.stdout);
                    update.notices.push((
                        Level::Info,
                        match devices.len() {
                            0 => "no MicroPython devices found".to_string(),
                            1 => format!("found {}", devices[0].label()),
                            count => format!("found {count} devices — press 'd' to choose"),
                        },
                    ));
                    update.device_scan = Some(Ok(devices));
                }
            },

            Request::List(path) => {
                // A stale reply for a directory the user already left must not
                // overwrite the pane they are looking at now.
                let current = *path == self.device_path;
                match failure {
                    Some(error) => {
                        update
                            .notices
                            .push((Level::Error, format!("{path}: {error}")));
                        if current {
                            self.device_state = PaneState::Failed(error);
                        }
                    }
                    None => {
                        let listing = parse::parse_listing(&output.stdout);
                        if !listing.unparsed.is_empty() {
                            update.notices.push((
                                Level::Warn,
                                format!(
                                    "{} line(s) of `{} fs ls` output were not understood",
                                    listing.unparsed.len(),
                                    commands::PROGRAM
                                ),
                            ));
                        }
                        let mut entries = listing.entries;
                        files::sort_remote(&mut entries);
                        update
                            .notices
                            .push((Level::Success, format!("{path}: {} entries", entries.len())));
                        self.cache.insert(path.clone(), entries);
                        if current {
                            self.device_state = PaneState::Ready;
                            self.clamp_cursors();
                        }
                    }
                }
            }

            Request::Hash { name, local_digest } => match failure {
                Some(error) => update
                    .notices
                    .push((Level::Error, format!("{name}: {error}"))),
                None => match parse::parse_sha256(&output.stdout) {
                    Some(remote_digest) => {
                        let identical = remote_digest == *local_digest;
                        self.verdicts.insert(name.clone(), identical);
                        update.notices.push((
                            if identical {
                                Level::Success
                            } else {
                                Level::Warn
                            },
                            format!(
                                "{name}: contents {}",
                                if identical { "identical" } else { "differ" }
                            ),
                        ));
                    }
                    None => update.notices.push((
                        Level::Warn,
                        format!("{name}: could not read a digest from the device"),
                    )),
                },
            },
        }
    }

    /// Whether a device command is currently running.
    pub fn is_busy(&self) -> bool {
        self.in_flight.is_some()
    }

    fn clamp_cursors(&mut self) {
        self.local_cursor = self
            .local_cursor
            .min(self.visible_local().len().saturating_sub(1));
        self.device_cursor = self
            .device_cursor
            .min(self.visible_device().len().saturating_sub(1));
    }
}

/// What a process event changed.
#[derive(Default)]
pub struct BrowserUpdate {
    pub notices: Vec<Notice>,
    /// Present when a device scan finished; the caller owns device state.
    pub device_scan: Option<Result<Vec<crate::device::DeviceInfo>, String>>,
}

/// Hashes a local file with the same algorithm the device uses.
fn hash_local(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};

    let metadata = std::fs::metadata(path).map_err(|source| format!("cannot read: {source}"))?;
    if metadata.is_dir() {
        return Err("is a directory".to_string());
    }
    if metadata.len() > MAX_LOCAL_HASH_BYTES {
        return Err(format!(
            "too large to hash on the UI thread ({} MiB)",
            metadata.len() / (1024 * 1024)
        ));
    }

    let contents = std::fs::read(path).map_err(|source| format!("cannot read: {source}"))?;
    let digest = Sha256::digest(&contents);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary local directory containing `main.py` and `lib/`.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("chiptui-browser-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(root.join("lib")).unwrap();
            std::fs::write(root.join("main.py"), "print('hi')\n").unwrap();
            std::fs::write(root.join(".hidden"), "x").unwrap();
            Self { root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn lists_the_local_directory_on_creation() {
        let fixture = Fixture::new("list");
        let browser = Browser::new(&fixture.root);

        let names: Vec<&str> = browser
            .visible_local()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["lib", "main.py"],
            "dotfiles hidden, directories first"
        );
    }

    #[test]
    fn toggling_hidden_reveals_dotfiles() {
        let fixture = Fixture::new("hidden");
        let mut browser = Browser::new(&fixture.root);
        assert_eq!(browser.visible_local().len(), 2);

        browser.toggle_hidden();
        assert_eq!(browser.visible_local().len(), 3);
    }

    #[test]
    fn entering_a_local_directory_moves_the_pane() {
        let fixture = Fixture::new("enter");
        let mut browser = Browser::new(&fixture.root);
        let mut processes = ProcessManager::new();

        browser.cursor_to(0); // lib/
        browser.enter(&mut processes, None);

        assert_eq!(browser.local_path, fixture.root.join("lib"));
        assert!(browser.visible_local().is_empty());

        browser.ascend(&mut processes, None);
        assert_eq!(browser.local_path, fixture.root);
    }

    #[test]
    fn entering_a_file_does_nothing() {
        let fixture = Fixture::new("file");
        let mut browser = Browser::new(&fixture.root);
        let mut processes = ProcessManager::new();

        browser.cursor_to(1); // main.py
        browser.enter(&mut processes, None);
        assert_eq!(browser.local_path, fixture.root);
    }

    #[test]
    fn an_unreadable_local_directory_is_reported_not_fatal() {
        let mut browser = Browser::new("/nonexistent-chiptui-path");
        assert!(browser.local_error.is_some());
        assert!(browser.visible_local().is_empty());

        browser.reload_local();
        assert!(browser.local_error.is_some());
    }

    #[test]
    fn device_requests_are_serialised() {
        let fixture = Fixture::new("queue");
        let mut browser = Browser::new(&fixture.root);
        // `true` never resolves a device, but it exists on every unix box and
        // exits immediately, which is all this test needs.
        let mut processes = ProcessManager::new();

        browser.scan_devices(&mut processes, None);
        browser.focus = Side::Device;
        browser.load_device(&mut processes, None, false);

        assert!(browser.is_busy());
        assert_eq!(browser.queue.len(), 1, "the second request waits its turn");
    }

    #[test]
    fn cursor_movement_is_clamped_per_pane() {
        let fixture = Fixture::new("cursor");
        let mut browser = Browser::new(&fixture.root);

        browser.move_cursor(50);
        assert_eq!(browser.local_cursor, 1);
        browser.move_cursor(-50);
        assert_eq!(browser.local_cursor, 0);

        // The device pane is empty, so its cursor cannot move.
        browser.toggle_focus();
        browser.move_cursor(3);
        assert_eq!(browser.device_cursor, 0);
    }

    #[test]
    fn hashing_a_local_file_matches_a_known_digest() {
        let fixture = Fixture::new("hash");
        let digest = hash_local(&fixture.root.join("main.py")).unwrap();

        // sha256 of "print('hi')\n"
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

        let repeat = hash_local(&fixture.root.join("main.py")).unwrap();
        assert_eq!(digest, repeat, "hashing is deterministic");
    }

    #[test]
    fn hashing_rejects_directories_and_missing_files() {
        let fixture = Fixture::new("hash-err");
        assert!(
            hash_local(&fixture.root.join("lib"))
                .unwrap_err()
                .contains("directory")
        );
        assert!(hash_local(&fixture.root.join("nope.py")).is_err());
    }
}
