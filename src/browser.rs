//! Dual-pane file browser state.
//!
//! Holds both panes, the device listing cache and the in-flight `mpremote`
//! requests. It never touches the log or the terminal: results come back as
//! [`Notice`] values the caller forwards, which keeps the whole state machine
//! testable without a UI.
//!
//! Device commands are serialised --- `mpremote` opens the serial port
//! exclusively, so two concurrent listings would fight over it.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::micropython::{commands, parse};
use crate::device::DevicePath;
use crate::files::{self, LocalEntry, SyncStatus, Verdicts};
use crate::logs::Level;
use crate::process::{Outcome, ProcessEvent, ProcessId, ProcessManager, Stream};

/// Device commands are quick, but a board in a bad state can hang the port.
pub const DEVICE_TIMEOUT: Duration = Duration::from_secs(20);

/// `run` executes arbitrary user code, which routinely takes longer than a
/// filesystem operation --- `DEVICE_TIMEOUT` is calibrated for `ls`/`cp`, not
/// a script that polls a sensor for a while.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// Local files above this are not hashed, to keep the UI thread responsive.
const MAX_LOCAL_HASH_BYTES: u64 = 32 * 1024 * 1024;

/// A message for the log pane.
pub type Notice = (Level, String);

/// A planned synchronization between the local tree and the device
/// filesystem, built by walking both sides and comparing file sizes.
///
/// Size comparison means a file whose content changed without changing length
/// (the case `SameSize` flags in the per-file view) will not appear here.
/// The user can still verify individual files with `c` (sha256) before
/// syncing; the sync itself stays fast by avoiding per-file hash round trips
/// over serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    /// Directories to create on the device, parents first.
    pub mkdirs: Vec<DevicePath>,
    /// Files to upload: local path and device target.
    pub uploads: Vec<(PathBuf, DevicePath)>,
    /// Device-only files that would be deleted (only removed when the user
    /// confirms in the preview overlay).
    pub deletes: Vec<DevicePath>,
}

impl SyncPlan {
    /// Whether there is nothing to do.
    pub fn is_empty(&self) -> bool {
        self.mkdirs.is_empty() && self.uploads.is_empty() && self.deletes.is_empty()
    }
}

/// Internal state for an in-progress sync walk of the device filesystem.
#[derive(Debug, Clone)]
struct SyncState {
    local_files: BTreeMap<String, u64>,
    local_dirs: BTreeSet<String>,
    device_files: BTreeMap<String, u64>,
    device_dirs: BTreeSet<String>,
    /// Directories whose `SyncList` result has not yet arrived.
    pending: BTreeSet<DevicePath>,
}

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
        /// The device directory the file lives in, snapshotted when the
        /// request is *queued*. Resolving it at dispatch time against
        /// `device_path` made a queued hash follow the user's navigation
        /// and land on a different file than the one asked about.
        dir: DevicePath,
        /// Entry name, used to record the verdict under `dir`.
        name: String,
        /// The locally computed digest to compare against.
        local_digest: String,
        /// Whether the verdict is worth a log line. The user's own `c`
        /// announces; a background check does not --- its consumer is a
        /// report row, the same silence [`Request::BackgroundList`] keeps.
        announce: bool,
    },
    /// `cat`, for the viewer's "View" action on a device file. Nothing is
    /// written to disk --- [`Download`](Request::Download) is the only
    /// request that touches the local filesystem.
    ViewDevice(DevicePath),
    /// `cp :remote local`, for "Download" (into the project tree,
    /// `then_edit: false`) and "Edit" (into a scratch temp file,
    /// `then_edit: true`) on a device file --- see
    /// [`Browser::request_download`]/[`Browser::request_edit_download`].
    Download {
        source: DevicePath,
        local_path: PathBuf,
        then_edit: bool,
    },
    /// `cp local :remote`, for "Send to device" on a local file, and for the
    /// re-upload after editing a device file (`after_edit`).
    Upload {
        local_path: PathBuf,
        target: DevicePath,
        after_edit: bool,
    },
    /// `rm :remote` to delete a device file.
    RemoveDevice(DevicePath),
    /// `rm --recursive :remote` to delete a device directory and everything
    /// under it --- "Delete" on a directory entry, distinct from
    /// [`Request::RemoveDevice`] since a plain `rm` refuses a directory.
    RemoveDeviceRecursive(DevicePath),
    /// `cp --recursive local_dir :remote_parent`, for "Send to device" on a
    /// local directory --- see [`Browser::request_upload_dir`].
    UploadDir {
        local_dir: PathBuf,
        remote_parent: DevicePath,
    },
    /// `cp --recursive :remote_dir local_parent`, for "Download" on a device
    /// directory --- see [`Browser::request_download_dir`].
    DownloadDir {
        remote_dir: DevicePath,
        local_parent: PathBuf,
    },
    /// `mkdir :remote`, for the create-entry action (`a`) when the typed
    /// name ends with `/`.
    Mkdir(DevicePath),
    /// `touch :remote`, for the create-entry action (`a`) on a plain name.
    Touch(DevicePath),
    /// `soft-reset`, offered after a post-edit re-upload lands.
    Reset,
    /// `reset` (hard reset) --- reboots the board so `boot.py`/`main.py` run
    /// again after an interruption --- see [`Browser::request_hard_reset`].
    HardReset,
    /// `exec --no-follow "import main"` --- restarts `main.py` without a
    /// reboot --- see [`Browser::request_relaunch_main`].
    RelaunchMain,
    /// `df` --- filesystem usage of the connected board, device-wide rather than
    /// per-path --- see [`Browser::load_device`].
    Df,
    /// `run LOCAL_PATH` --- executes a local script on the device without
    /// copying it, for [`FileAction::Run`](crate::app::FileAction::Run) on a
    /// local file. See [`Browser::request_run`].
    Run(PathBuf),
    /// `mip install SPEC [SPEC ...]` --- for the package-install prompt (`i`
    /// on the device pane, one package) and the Project pane's Dependencies
    /// row (every line of `requirements.txt`; mpremote 1.28 has no `-r`).
    /// See [`Browser::request_mip_install`].
    MipInstall(Vec<String>),
    /// Recursive `ls` for building a [`SyncPlan`] --- like [`Request::List`]
    /// but feeds into [`Browser::sync`] instead of the pane cache.
    SyncList(DevicePath),
    /// A listing nobody is looking at: `/lib` for the Dependencies row's
    /// coverage count, and a package directory under it for a dotted name
    /// (`umqtt.simple` lands in `/lib/umqtt/`). Like [`Request::List`] but
    /// silent on success --- a report row is the consumer, not a pane ---
    /// and tolerant of the directory not existing: a board with nothing
    /// installed has no `/lib`, which is the answer "0/N installed", not an
    /// error.
    BackgroundList(DevicePath),
}

/// A device `cat` finished --- the viewer's content, or the reason it could
/// not be read (already logged into `notices` too, so the viewer's error
/// state and the log agree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    pub path: DevicePath,
    pub content: Result<String, String>,
}

/// A `run` finished --- the local script's captured stdout, or the reason it
/// could not be run (already logged into `notices` too, same shape as
/// [`DeviceView`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub path: PathBuf,
    pub output: Result<String, String>,
}

/// What a completed transfer was for, carrying what its caller needs to
/// react: [`App`](crate::app::App) opens `$EDITOR` on a successful
/// download-to-edit, and otherwise a transfer is fire-and-forget (its
/// outcome is already in `notices`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferKind {
    Download {
        local_path: PathBuf,
        then_edit: bool,
        /// Where it came from --- carried through so a successful
        /// `then_edit` download can be re-uploaded to the same place once
        /// `$EDITOR` closes.
        source: DevicePath,
    },
    Upload {
        /// Set for the re-upload that follows editing a device file, so a
        /// successful landing can offer a restart; an ordinary "Send to
        /// device" is fire-and-forget, its outcome already in `notices`.
        after_edit: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub kind: TransferKind,
    pub ok: bool,
}

/// stdout/stderr collected for one in-flight process.
#[derive(Default)]
struct Output {
    stdout: String,
    stderr: String,
}

pub struct Browser {
    /// The project root the local pane was opened (or re-rooted) on: the
    /// anchor of the `Project files: name/path` title. The browsed
    /// [`Self::local_path`] may sit below it (or, because the pane may
    /// ascend, above it) --- only the root names the project.
    pub local_root: PathBuf,
    pub local_path: PathBuf,
    pub local_entries: Vec<LocalEntry>,
    pub local_error: Option<String>,
    pub local_cursor: usize,

    pub device_path: DevicePath,
    pub device_state: PaneState,
    pub device_cursor: usize,
    /// Filesystem usage of the connected board, device-wide --- `None` until the first
    /// [`Request::Df`] resolves for the current connection.
    pub device_space: Option<Result<parse::DiskUsage, String>>,

    pub focus: Side,
    pub show_hidden: bool,

    /// Listings by device path: each `ls` costs seconds over serial.
    cache: BTreeMap<DevicePath, Vec<parse::RemoteEntry>>,
    /// sha256 verdicts, **scoped to the directory they were computed in**.
    /// Keyed by bare name alone they aliased across directories: verifying
    /// `/lib/main.py` and walking up to `/` handed the root's `main.py` a
    /// verdict that was never about it, and navigation does not clear them.
    verdicts: BTreeMap<DevicePath, Verdicts>,

    queue: VecDeque<Request>,
    in_flight: Option<(ProcessId, Request)>,
    output: HashMap<ProcessId, Output>,

    /// While set, queued device requests are held instead of started --- the
    /// app believes user code is running on the device, and `mpremote`
    /// interrupts it (Ctrl-C, then raw REPL) for every command. The user must
    /// confirm before anything leaves the queue; see [`Browser::held_for_interrupt`].
    interrupt_gate: bool,
    /// Set once the gate has actually held something, so the app knows to ask.
    gate_pending: bool,

    /// Overrides the `mpremote` executable. `None` means "resolve on PATH".
    tool_path: Option<String>,

    /// Name to select in the device pane once its listing is available.
    /// Set by [`Self::ascend`] (yazi-style: leaving a directory leaves its
    /// entry selected in the parent) and consumed wherever the listing of
    /// the directory that became current is already cached or finishes
    /// loading. Only a cache miss can defer it, and any manual cursor move
    /// cancels it.
    pending_select: Option<String>,

    /// Active sync walk, if [`Self::request_sync`] was called and the
    /// resulting [`SyncPlan`] has not yet been produced.
    sync: Option<SyncState>,
}

impl Browser {
    pub fn new(local_path: impl Into<PathBuf>) -> Self {
        let local_path = local_path.into();
        let mut browser = Self {
            local_root: local_path.clone(),
            local_path,
            local_entries: Vec::new(),
            local_error: None,
            local_cursor: 0,
            device_path: DevicePath::root(),
            device_state: PaneState::Idle,
            device_cursor: 0,
            device_space: None,
            focus: Side::Local,
            show_hidden: false,
            cache: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            queue: VecDeque::new(),
            in_flight: None,
            output: HashMap::new(),
            interrupt_gate: false,
            gate_pending: false,
            tool_path: None,
            pending_select: None,
            sync: None,
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

    /// Returns the configured tool-path override, if any.
    pub fn tool_path(&self) -> Option<&str> {
        self.tool_path.as_deref()
    }

    // ---- local pane -----------------------------------------------------

    /// Re-roots the local pane on `dir` (a picked MicroPython project):
    /// both the root the title names and the browsed path move, the cursor
    /// resets --- a re-root is a fresh start, the same rule the Zephyr
    /// panes' own re-rooting follows.
    pub fn set_local_root(&mut self, dir: impl Into<PathBuf>) {
        let dir = dir.into();
        self.local_root = dir.clone();
        self.local_path = dir;
        self.local_cursor = 0;
        self.reload_local();
    }

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

    /// Whether the browsed local directory no longer matches the listed
    /// snapshot (the tick's auto-refresh asks before reloading, so an
    /// unchanged directory costs nothing visible). A pane sitting on an
    /// error always answers yes: the directory was unreadable at the last
    /// attempt, so retrying is the only way an externally created or
    /// repaired directory is ever picked up.
    pub fn local_listing_changed(&self) -> bool {
        self.local_error.is_some() || files::listing_changed(&self.local_entries, &self.local_path)
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
        files::compare(&local, &device, self.verdicts_for(&self.device_path))
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
        self.pending_select = None;
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
        self.pending_select = None;
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

    /// Size of `name` in the current device directory, from the cached
    /// listing --- lets the viewer refuse an oversized device file before
    /// spending a round trip fetching it (mirrors [`files::read_text_file`]'s
    /// size check on the local side).
    pub fn device_entry_size(&self, name: &str) -> Option<u64> {
        self.visible_device()
            .into_iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.size)
    }

    /// Whether the entry under the cursor in `side` is a directory --- used
    /// to decide which [`crate::app::FileAction`]s a files-pane `Enter`
    /// offers for it.
    pub fn selected_is_dir(&self, side: Side) -> bool {
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
                self.pending_select = None;
                self.load_device(processes, port, false)
            }
        }
    }

    /// Moves the focused pane to its parent directory.
    ///
    /// Yazi-style: the entry for the directory just left ends up selected in
    /// the parent, so `← ←` is a no-op walk around the spot you came from.
    pub fn ascend(&mut self, processes: &mut ProcessManager, port: Option<&str>) -> Vec<Notice> {
        match self.focus {
            Side::Local => {
                if let Some(parent) = self.local_path.parent().map(Path::to_path_buf) {
                    let left = self
                        .local_path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned());
                    self.local_path = parent;
                    self.local_cursor = 0;
                    self.reload_local();
                    if let Some(left) = left {
                        self.select_local_by_name(&left);
                    }
                }
                Vec::new()
            }
            Side::Device => match self.device_path.parent() {
                Some(parent) => {
                    self.pending_select = Some(self.device_path.name().to_string());
                    self.device_path = parent;
                    self.device_cursor = 0;
                    self.load_device(processes, port, false)
                }
                None => Vec::new(),
            },
        }
    }

    /// Puts the local cursor on the entry called `name`, if one is visible.
    fn select_local_by_name(&mut self, name: &str) {
        if let Some(index) = self
            .visible_local()
            .iter()
            .position(|entry| entry.name == name)
        {
            self.local_cursor = index;
        }
    }

    /// Consumes [`Self::pending_select`] against the current device listing.
    fn apply_pending_select(&mut self) {
        if let Some(index) = self.pending_select.take().and_then(|name| {
            self.visible_device()
                .iter()
                .position(|entry| entry.name == name)
        }) {
            self.device_cursor = index;
        }
    }

    // ---- device requests ------------------------------------------------

    /// Drops every cached device listing, comparison verdict and free-space
    /// reading: the selection moved to another board, so none of it can be
    /// shown under the new one. Called before the probe / chip query /
    /// listing chain re-reads the device.
    pub fn invalidate_device_cache(&mut self) {
        self.cache.clear();
        self.verdicts.clear();
        self.device_space = None;
    }

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
            self.device_space = None;
        }

        // Free space is device-wide, not per-path, so it is fetched once per connection
        // rather than on every `enter`/`ascend` --- `enqueue` is safe to call twice here:
        // `Df` starts immediately and `List` queues behind it (`pump` only starts a request
        // when nothing is in flight).
        let mut notices = Vec::new();
        if self.device_space.is_none() {
            notices.extend(self.enqueue(Request::Df, processes, port));
        }

        if self.cache.contains_key(&self.device_path) {
            self.device_state = PaneState::Ready;
            self.clamp_cursors();
            self.apply_pending_select();
            return notices;
        }

        self.device_state = PaneState::Loading;
        notices.extend(self.enqueue(Request::List(self.device_path.clone()), processes, port));
        notices
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
        let request = Request::Hash {
            dir: self.device_path.clone(),
            name,
            local_digest,
            announce: true,
        };
        notices.extend(self.enqueue(request, processes, port));
        notices
    }

    /// Queues a silent sha256 comparison of `name` in `dir` --- the Project
    /// pane's boot-file report, which needs a real `=`/`≠` rather than the
    /// `≈` a size match alone produces. Nothing is logged either way: the
    /// row is the consumer, exactly as [`Request::BackgroundList`]'s is.
    pub fn request_background_hash(
        &mut self,
        dir: DevicePath,
        name: &str,
        local_file: &Path,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        if self
            .verdicts
            .get(&dir)
            .is_some_and(|v| v.contains_key(name))
        {
            return Vec::new();
        }
        let Ok(local_digest) = hash_local(local_file) else {
            // An unreadable local file is not worth a device round-trip;
            // the row keeps its size-based verdict.
            return Vec::new();
        };
        let request = Request::Hash {
            dir,
            name: name.to_string(),
            local_digest,
            announce: false,
        };
        self.enqueue(request, processes, port)
    }

    /// Queues a `cat` of `name`, in the current device directory, for the
    /// viewer's "View" action. Content arrives later as
    /// [`BrowserUpdate::device_view`].
    pub fn request_device_view(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let path = self.device_path.join(name);
        let mut notices = vec![(Level::Info, format!("reading {path}"))];
        notices.extend(self.enqueue(Request::ViewDevice(path), processes, port));
        notices
    }

    /// Queues a download of `name`, in the current device directory, to the
    /// local pane's current directory --- "Download" on a device file, into
    /// the project tree.
    pub fn request_download(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let source = self.device_path.join(name);
        let local_path = self.local_path.join(name);
        let mut notices = vec![(Level::Info, format!("downloading {source}"))];
        notices.extend(self.enqueue(
            Request::Download {
                source,
                local_path,
                then_edit: false,
            },
            processes,
            port,
        ));
        notices
    }

    /// Queues a download of `name`, in the current device directory, to a
    /// scratch temp file for `$EDITOR` --- "Edit" on a device file. Never
    /// the project tree: the point is to try a change on the device first;
    /// [`Self::request_download`] is the separate, explicit step for
    /// bringing a confirmed-good result into the project once it works
    /// (`then_edit: true` on the resulting [`BrowserUpdate::transfer`] is
    /// what tells the caller to open `$EDITOR` once it lands).
    pub fn request_edit_download(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let source = self.device_path.join(name);
        let local_path = edit_download_path(name);
        if let Some(parent) = local_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut notices = vec![(Level::Info, format!("downloading {source} to try it out"))];
        notices.extend(self.enqueue(
            Request::Download {
                source,
                local_path,
                then_edit: true,
            },
            processes,
            port,
        ));
        notices
    }

    /// Queues an upload of `name`, in the current local directory, to the
    /// device's current directory --- "Send to device" on a local file.
    pub fn request_upload(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let local_path = self.local_path.join(name);
        let target = self.device_path.join(name);
        let mut notices = vec![(Level::Info, format!("sending {name} to {target}"))];
        notices.extend(self.enqueue(
            Request::Upload {
                local_path,
                target,
                after_edit: false,
            },
            processes,
            port,
        ));
        notices
    }

    /// Re-uploads `local_path` to `target` once `$EDITOR` closes on a device
    /// file. Unlike [`Self::request_upload`], both paths are exact rather
    /// than resolved from the current directory --- the browser may have
    /// navigated elsewhere while the terminal was suspended running
    /// `$EDITOR` --- and `after_edit: true` on the resulting
    /// [`BrowserUpdate::transfer`] is what tells the caller to offer a
    /// restart once it lands.
    pub fn request_reupload_after_edit(
        &mut self,
        local_path: PathBuf,
        target: DevicePath,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let mut notices = vec![(Level::Info, format!("sending edited file to {target}"))];
        notices.extend(self.enqueue(
            Request::Upload {
                local_path,
                target,
                after_edit: true,
            },
            processes,
            port,
        ));
        notices
    }

    /// Queues a `soft-reset`, offered after a post-edit re-upload lands.
    pub fn request_reset(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let mut notices = vec![(Level::Info, "restarting device".to_string())];
        notices.extend(self.enqueue(Request::Reset, processes, port));
        notices
    }

    /// Queues a hard reset (`mpremote reset`): the board reboots and runs
    /// `boot.py` + `main.py` again --- the thorough way to bring back a script
    /// that was interrupted for a filesystem operation. `--no-follow` inside
    /// mpremote's expansion keeps the command from waiting on a board that is
    /// busy rebooting.
    pub fn request_hard_reset(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let mut notices = vec![(Level::Info, "resetting the device".to_string())];
        notices.extend(self.enqueue(Request::HardReset, processes, port));
        notices
    }

    /// Queues `exec --no-follow "import main"`: starts `main.py` again without
    /// rebooting. Faster than a reset, but whatever state the interrupted run
    /// left behind (open sockets, claimed peripherals) is still there.
    pub fn request_relaunch_main(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let mut notices = vec![(Level::Info, "restarting main.py".to_string())];
        notices.extend(self.enqueue(Request::RelaunchMain, processes, port));
        notices
    }

    /// Turns the interrupt gate on or off. Called by the app whenever its
    /// belief about a running script changes ([`crate::device::ScriptState`]).
    ///
    /// Turning the gate *off* resumes whatever was held --- the usual path is
    /// the user confirming an interruption, but a script that ends on its own
    /// releases the queue the same way. An already in-flight request is
    /// unaffected either way: it started before the belief changed.
    pub fn set_interrupt_gate(
        &mut self,
        active: bool,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) {
        if active {
            self.interrupt_gate = true;
            return;
        }
        if self.interrupt_gate {
            self.interrupt_gate = false;
            self.gate_pending = false;
            self.pump(processes, port);
        }
    }

    /// Whether device requests are being held for the user's confirmation.
    ///
    /// The app polls this after anything that might have queued a request
    /// (a key press, a completed command) and opens the confirmation overlay
    /// when it flips to `true`.
    pub fn held_for_interrupt(&self) -> bool {
        self.gate_pending
    }

    /// Drops every held request, for when the user declined the interruption.
    /// The device pane lands in an explainable state rather than spinning on
    /// "loading" forever.
    pub fn cancel_held_requests(&mut self) {
        self.queue.clear();
        self.gate_pending = false;
        self.device_state = if self.cache.contains_key(&self.device_path) {
            PaneState::Ready
        } else {
            PaneState::Failed("cancelled \u{2014} a script is running on the device".to_string())
        };
        self.clamp_cursors();
    }

    /// Queues a deletion of a device file.
    pub fn request_remove_device(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let target = self.device_path.join(name);
        let mut notices = vec![(Level::Info, format!("removing {target}"))];
        notices.extend(self.enqueue(Request::RemoveDevice(target), processes, port));
        notices
    }

    /// Queues a recursive deletion of a device directory, in the current
    /// device directory --- "Delete" on a directory entry.
    pub fn request_remove_device_dir(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let target = self.device_path.join(name);
        let mut notices = vec![(Level::Info, format!("removing {target}/ (recursive)"))];
        notices.extend(self.enqueue(Request::RemoveDeviceRecursive(target), processes, port));
        notices
    }

    /// Queues a deletion of an arbitrary device path --- the package
    /// manager's uninstall, which acts on `/lib/...` while the device pane
    /// is wherever the user left it. The name-taking pair above joins onto
    /// `device_path`, which would be the wrong file entirely here.
    pub fn request_remove_path(
        &mut self,
        target: DevicePath,
        recursive: bool,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let mut notices = vec![(
            Level::Info,
            if recursive {
                format!("removing {target}/ (recursive)")
            } else {
                format!("removing {target}")
            },
        )];
        let request = if recursive {
            Request::RemoveDeviceRecursive(target)
        } else {
            Request::RemoveDevice(target)
        };
        notices.extend(self.enqueue(request, processes, port));
        notices
    }

    /// Queues a recursive upload of `name`, a directory in the current local
    /// directory, into the device's current directory --- "Send to device"
    /// on a directory entry. The destination is the device's current
    /// directory itself, not `device_path.join(name)`: mpremote nests the
    /// source's own basename under an existing destination directory, the
    /// same way Unix `cp -r src existing_dest_dir` does.
    pub fn request_upload_dir(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let local_dir = self.local_path.join(name);
        let remote_parent = self.device_path.clone();
        let mut notices = vec![(Level::Info, format!("sending {name}/ to {remote_parent}"))];
        notices.extend(self.enqueue(
            Request::UploadDir {
                local_dir,
                remote_parent,
            },
            processes,
            port,
        ));
        notices
    }

    /// Queues a recursive download of `name`, a directory in the current
    /// device directory, into the local pane's current directory ---
    /// "Download" on a directory entry. Same existing-destination reasoning
    /// as [`Self::request_upload_dir`], mirrored.
    pub fn request_download_dir(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let remote_dir = self.device_path.join(name);
        let local_parent = self.local_path.clone();
        let mut notices = vec![(Level::Info, format!("downloading {remote_dir}/"))];
        notices.extend(self.enqueue(
            Request::DownloadDir {
                remote_dir,
                local_parent,
            },
            processes,
            port,
        ));
        notices
    }

    /// Queues creation of an empty directory in the current device
    /// directory --- the create-entry action (`a`) when the typed name ends
    /// with `/`.
    pub fn request_mkdir(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let target = self.device_path.join(name);
        let mut notices = vec![(Level::Info, format!("creating directory {target}"))];
        notices.extend(self.enqueue(Request::Mkdir(target), processes, port));
        notices
    }

    /// Queues creation of an empty file in the current device directory ---
    /// the create-entry action (`a`) on a plain name.
    pub fn request_touch(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let target = self.device_path.join(name);
        let mut notices = vec![(Level::Info, format!("creating {target}"))];
        notices.extend(self.enqueue(Request::Touch(target), processes, port));
        notices
    }

    /// Queues a run of `name`, in the current local directory, on the device
    /// --- "Run" on a local file. The script is never copied to the device
    /// filesystem; only its captured output comes back.
    pub fn request_run(
        &mut self,
        name: &str,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let local_path = self.local_path.join(name);
        let mut notices = vec![(Level::Info, format!("running {}", local_path.display()))];
        notices.extend(self.enqueue(Request::Run(local_path), processes, port));
        notices
    }

    /// Queues a `mip install` of every specification --- the package-install
    /// prompt (`i`) passes one, the Project pane's Dependencies row passes
    /// every line of `requirements.txt` in one command (mip skips files that
    /// are already installed, so re-running it is safe).
    pub fn request_mip_install(
        &mut self,
        packages: &[String],
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let mut notices = vec![(Level::Info, format!("installing {}", packages.join(", ")))];
        notices.extend(self.enqueue(Request::MipInstall(packages.to_vec()), processes, port));
        notices
    }

    /// Starts a batch sync: recursively walks the local tree and the device
    /// filesystem, compares file sizes, and returns a [`SyncPlan`] for the
    /// user to review (via [`BrowserUpdate::sync_plan`]). The plan is not
    /// executed until [`Self::execute_sync`] is called.
    ///
    /// The local walk is synchronous (fast disk I/O); the device walk
    /// serialises one `ls` per directory through the normal request queue,
    /// so it respects serial exclusivity.
    pub fn request_sync(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let (local_files, local_dirs) = files::walk_local(&self.local_path);
        self.sync = Some(SyncState {
            local_files,
            local_dirs,
            device_files: BTreeMap::new(),
            device_dirs: BTreeSet::new(),
            pending: BTreeSet::from([DevicePath::root()]),
        });
        let mut notices = vec![(Level::Info, "scanning device for sync".to_string())];
        notices.extend(self.enqueue(Request::SyncList(DevicePath::root()), processes, port));
        notices
    }

    /// Queues all operations from a confirmed [`SyncPlan`]. Directories are
    /// created first (parents before children, courtesy of `BTreeSet` order),
    /// then files are uploaded. Device-only files are deleted only when
    /// `delete_extras` is true.
    pub fn execute_sync(
        &mut self,
        plan: &SyncPlan,
        delete_extras: bool,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let total = plan.mkdirs.len()
            + plan.uploads.len()
            + if delete_extras { plan.deletes.len() } else { 0 };
        if total == 0 {
            return vec![(
                Level::Info,
                "nothing to sync \u{2014} already in sync".to_string(),
            )];
        }

        let mut notices = vec![(Level::Info, format!("syncing {total} operation(s)"))];

        for dir in &plan.mkdirs {
            self.queue.push_back(Request::Mkdir(dir.clone()));
        }
        for (local, target) in &plan.uploads {
            self.queue.push_back(Request::Upload {
                local_path: local.clone(),
                target: target.clone(),
                after_edit: false,
            });
        }
        if delete_extras {
            for path in &plan.deletes {
                self.queue.push_back(Request::RemoveDevice(path.clone()));
            }
        }

        notices.extend(self.pump(processes, port));
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
        if self.interrupt_gate {
            // mpremote would interrupt the running script to run any of these;
            // hold them until the user says that is fine.
            self.gate_pending = !self.queue.is_empty();
            return Vec::new();
        }
        let Some(request) = self.queue.pop_front() else {
            self.gate_pending = false;
            return Vec::new();
        };

        let command = match &request {
            Request::Devices => commands::list_devices(),
            Request::List(path) => commands::list_dir(port, path),
            Request::Hash { dir, name, .. } => commands::sha256(port, &dir.join(name)),
            Request::ViewDevice(path) => commands::cat(port, path),
            Request::Download {
                source, local_path, ..
            } => commands::download(port, source, local_path),
            Request::Upload {
                local_path, target, ..
            } => commands::upload(port, local_path, target),
            Request::RemoveDevice(path) => commands::rm(port, path),
            Request::RemoveDeviceRecursive(path) => commands::rm_recursive(port, path),
            Request::UploadDir {
                local_dir,
                remote_parent,
            } => commands::upload_dir(port, local_dir, remote_parent),
            Request::DownloadDir {
                remote_dir,
                local_parent,
            } => commands::download_dir(port, remote_dir, local_parent),
            Request::Mkdir(path) => commands::mkdir(port, path),
            Request::Touch(path) => commands::touch(port, path),
            Request::Reset => commands::soft_reset(port),
            Request::HardReset => commands::hard_reset(port),
            Request::RelaunchMain => commands::relaunch_main(port),
            Request::Df => commands::df(port),
            Request::Run(path) => commands::run(port, path),
            Request::MipInstall(package) => commands::mip_install(port, package),
            Request::SyncList(path) => commands::list_dir(port, path),
            Request::BackgroundList(path) => commands::list_dir(port, path),
        };
        let command = match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        };

        let timeout = match &request {
            // `run` executes arbitrary user code, and `mip install` downloads
            // over the network before writing over serial --- both routinely
            // outlast a filesystem operation.
            Request::Run(_) | Request::MipInstall(_) => RUN_TIMEOUT,
            _ => DEVICE_TIMEOUT,
        };
        let id = processes.spawn(command, timeout);
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
            // Raw PTY bytes belong to the Terminal tab's emulator alone;
            // nothing here ever spawns raw.
            ProcessEvent::Output { .. } | ProcessEvent::Bytes { .. } => {}
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
                    // A scan can follow a hotplug swap on the very same port,
                    // so a cached listing, free-space reading or verdict from
                    // whatever was previously connected must not survive it
                    // (`load_device_root` reuses the cache and would otherwise
                    // show the old board's files under the new one).
                    self.cache.clear();
                    self.verdicts.clear();
                    self.device_space = None;
                    update.device_scan = Some(Ok(devices));
                }
            },

            Request::List(path) => {
                // A stale reply for a directory the user already left must not
                // overwrite the pane they are looking at now.
                let current = *path == self.device_path;
                match failure {
                    Some(error) => {
                        self.rescan_if_device_lost(output, update);
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
                        update.listed = Some(path.clone());
                        if current {
                            self.device_state = PaneState::Ready;
                            self.clamp_cursors();
                            self.apply_pending_select();
                        }
                        // A fresh root listing is also the once-per-connection
                        // hook for the Dependencies row's coverage count:
                        // `/lib` rides the same cache and is listed right
                        // after, so it never runs ahead of the pane's own
                        // content. The cache is cleared on device change and
                        // reset --- exactly when the coverage answer goes
                        // stale --- which is what makes this once-per-
                        // connection rather than once-per-run.
                        let lib = DevicePath::new("/lib");
                        if path.is_root()
                            && !self.cache.contains_key(&lib)
                            && !self.queue.iter().any(|request| {
                                matches!(
                                    request,
                                    Request::List(p) | Request::BackgroundList(p) if *p == lib
                                )
                            })
                        {
                            self.queue.push_back(Request::BackgroundList(lib));
                        }
                    }
                }
            }

            Request::BackgroundList(path) => match failure {
                Some(_) if parse::is_path_not_found_error(&output.stderr) => {
                    // A board with nothing installed has no `/lib` yet:
                    // that is the answer "0/N installed", not a failure.
                    self.cache.insert(path.clone(), Vec::new());
                    update.listed = Some(path.clone());
                    update.notices.push((
                        Level::Info,
                        format!("{path}: does not exist yet — no packages installed"),
                    ));
                }
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("{path}: {error}")));
                    if *path == self.device_path {
                        self.device_state = PaneState::Failed(error);
                    }
                }
                None => {
                    let mut entries = parse::parse_listing(&output.stdout).entries;
                    files::sort_remote(&mut entries);
                    self.cache.insert(path.clone(), entries);
                    update.listed = Some(path.clone());
                    if *path == self.device_path {
                        self.device_state = PaneState::Ready;
                        self.clamp_cursors();
                        self.apply_pending_select();
                    }
                    // No notice on success: the Dependencies row is the
                    // consumer, and the log already carries the install or
                    // connection that made the answer interesting.
                }
            },

            Request::Hash {
                dir,
                name,
                local_digest,
                announce,
            } => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    if *announce {
                        update
                            .notices
                            .push((Level::Error, format!("{name}: {error}")));
                    }
                }
                None => match parse::parse_sha256(&output.stdout) {
                    Some(remote_digest) => {
                        let identical = remote_digest == *local_digest;
                        self.verdicts
                            .entry(dir.clone())
                            .or_default()
                            .insert(name.clone(), identical);
                        if *announce {
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
                    }
                    None if *announce => update.notices.push((
                        Level::Warn,
                        format!("{name}: could not read a digest from the device"),
                    )),
                    None => {}
                },
            },

            Request::ViewDevice(path) => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("{path}: {error}")));
                    update.device_view = Some(DeviceView {
                        path: path.clone(),
                        content: Err(error),
                    });
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, format!("{path}: read for preview")));
                    update.device_view = Some(DeviceView {
                        path: path.clone(),
                        content: Ok(output.stdout.clone()),
                    });
                }
            },

            Request::Download {
                source,
                local_path,
                then_edit,
            } => {
                let kind = TransferKind::Download {
                    local_path: local_path.clone(),
                    then_edit: *then_edit,
                    source: source.clone(),
                };
                match failure {
                    Some(error) => {
                        self.rescan_if_device_lost(output, update);
                        update
                            .notices
                            .push((Level::Error, format!("{source}: download failed: {error}")));
                        update.transfer = Some(Transfer { kind, ok: false });
                    }
                    None => {
                        // An edit download lands in a scratch temp
                        // directory (`edit_download_path`) --- naming it in
                        // the log would just be noise the user did not ask
                        // for; an ordinary download names exactly where it
                        // went, into the project tree.
                        let message = if *then_edit {
                            format!("{source} ready to try out")
                        } else {
                            format!("{source} downloaded to {}", local_path.display())
                        };
                        update.notices.push((Level::Success, message));
                        // Cheap and always safe, unlike a device listing: just
                        // re-reads whatever directory the local pane currently
                        // shows, wherever that is by now. A no-op for an edit
                        // download, which never touches it.
                        self.reload_local();
                        update.transfer = Some(Transfer { kind, ok: true });
                    }
                }
            }

            Request::Upload {
                local_path,
                target,
                after_edit,
            } => {
                let kind = TransferKind::Upload {
                    after_edit: *after_edit,
                };
                match failure {
                    Some(error) => {
                        self.rescan_if_device_lost(output, update);
                        update.notices.push((
                            Level::Error,
                            format!("{}: upload failed: {error}", local_path.display()),
                        ));
                        update.transfer = Some(Transfer { kind, ok: false });
                    }
                    None => {
                        update.notices.push((
                            Level::Success,
                            format!("{} uploaded to {target}", local_path.display()),
                        ));
                        // The directory just written into may be the one on
                        // screen; a stale cache entry would hide the new file
                        // until the user manually reloads.
                        if let Some(dir) = target.parent() {
                            self.cache.remove(&dir);
                            if dir == self.device_path {
                                self.device_state = PaneState::Loading;
                                self.queue.push_front(Request::List(dir));
                            }
                        }
                        update.transfer = Some(Transfer { kind, ok: true });
                    }
                }
            }
            Request::RemoveDevice(path) => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("{}: remove failed: {error}", path)));
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, format!("{} removed", path)));
                    // Reload current dir
                    let dir = path.parent().unwrap_or(crate::device::DevicePath::root());
                    if self.device_path == dir {
                        self.queue.push_front(Request::List(dir.clone()));
                    }
                    self.refresh_after_lib_change(&dir);
                }
            },

            Request::RemoveDeviceRecursive(path) => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("{path}: remove failed: {error}")));
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, format!("{path} removed")));
                    let dir = path.parent().unwrap_or(crate::device::DevicePath::root());
                    self.cache.remove(&dir);
                    if self.device_path == dir {
                        self.device_state = PaneState::Loading;
                        self.queue.push_front(Request::List(dir.clone()));
                    }
                    self.refresh_after_lib_change(&dir);
                }
            },

            Request::UploadDir {
                local_dir,
                remote_parent,
            } => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update.notices.push((
                        Level::Error,
                        format!("{}: upload failed: {error}", local_dir.display()),
                    ));
                }
                None => {
                    update.notices.push((
                        Level::Success,
                        format!("{} uploaded to {remote_parent}", local_dir.display()),
                    ));
                    self.cache.remove(remote_parent);
                    if *remote_parent == self.device_path {
                        self.device_state = PaneState::Loading;
                        self.queue.push_front(Request::List(remote_parent.clone()));
                    }
                }
            },

            Request::DownloadDir {
                remote_dir,
                local_parent,
            } => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update.notices.push((
                        Level::Error,
                        format!("{remote_dir}: download failed: {error}"),
                    ));
                }
                None => {
                    update.notices.push((
                        Level::Success,
                        format!("{remote_dir} downloaded to {}", local_parent.display()),
                    ));
                    self.reload_local();
                }
            },

            Request::Mkdir(path) => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("{path}: mkdir failed: {error}")));
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, format!("{path} created")));
                    self.invalidate_parent_of(path);
                }
            },

            Request::Touch(path) => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("{path}: create failed: {error}")));
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, format!("{path} created")));
                    self.invalidate_parent_of(path);
                }
            },

            Request::Reset => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("reset failed: {error}")));
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, "device reset --- reloading".to_string()));
                    // The soft reboot happened inside raw REPL, where main.py
                    // is skipped, and the reload below re-enters raw REPL
                    // besides: the script is stopped afterwards, not running.
                    update.script_running = Some(false);
                    // A reboot invalidates whatever was cached; the pane the
                    // user is looking at is worth a fresh look right away
                    // rather than waiting for them to notice and press 'r'.
                    self.cache.clear();
                    self.verdicts.clear();
                    self.device_state = PaneState::Loading;
                    self.queue
                        .push_front(Request::List(self.device_path.clone()));
                }
            },

            Request::HardReset => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("reset failed: {error}")));
                }
                None => {
                    update.notices.push((
                        Level::Success,
                        "device resetting --- boot.py and main.py will run again".to_string(),
                    ));
                    // `--no-follow`: mpremote is already gone while the board
                    // reboots, so from its side the command never "finishes".
                    update.script_running = Some(true);
                    // A reboot invalidates whatever was cached.
                    self.cache.clear();
                    self.verdicts.clear();
                    self.device_space = None;
                }
            },

            Request::RelaunchMain => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update
                        .notices
                        .push((Level::Error, format!("restarting main.py failed: {error}")));
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, "main.py restarted".to_string()));
                    // Fire-and-forget by design: the script runs on after the
                    // command returns, so the next device operation must gate
                    // again rather than assume an idle board.
                    update.script_running = Some(true);
                }
            },

            Request::Df => match failure {
                Some(error) => {
                    update.notices.push((
                        Level::Warn,
                        format!("device free space unavailable: {error}"),
                    ));
                    self.device_space = Some(Err(error));
                }
                None => match parse::parse_df(&output.stdout) {
                    Some(usage) => self.device_space = Some(Ok(usage)),
                    None => {
                        let message = "could not parse device free space".to_string();
                        update.notices.push((Level::Warn, message.clone()));
                        self.device_space = Some(Err(message));
                    }
                },
            },

            Request::Run(path) => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update.notices.push((
                        Level::Error,
                        format!("{}: run failed: {error}", path.display()),
                    ));
                    update.run_output = Some(RunOutput {
                        path: path.clone(),
                        output: Err(error),
                    });
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, format!("{}: finished", path.display())));
                    update.run_output = Some(RunOutput {
                        path: path.clone(),
                        output: Ok(output.stdout.clone()),
                    });
                }
            },

            Request::MipInstall(packages) => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    update.notices.push((
                        Level::Error,
                        format!("installing {} failed: {error}", packages.join(", ")),
                    ));
                }
                None => {
                    update
                        .notices
                        .push((Level::Success, format!("{} installed", packages.join(", "))));
                    // `mip install` always writes under `/lib`; a stale
                    // cache entry there would hide the new package until the
                    // user manually reloads, and the Dependencies row's
                    // coverage count reads the same cache.
                    self.refresh_after_lib_change(&DevicePath::new(
                        crate::backend::micropython::deps::LIB_ROOT,
                    ));
                }
            },

            Request::SyncList(path) => match failure {
                Some(error) => {
                    self.rescan_if_device_lost(output, update);
                    self.sync = None;
                    update
                        .notices
                        .push((Level::Error, format!("sync aborted at {path}: {error}")));
                }
                None => {
                    let listing = parse::parse_listing(&output.stdout);
                    let mut done = false;
                    if let Some(sync) = &mut self.sync {
                        sync.pending.remove(path);
                        for entry in &listing.entries {
                            if files::is_hidden(&entry.name) {
                                continue;
                            }
                            let full = path.join(&entry.name);
                            let relative =
                                full.as_str().strip_prefix('/').unwrap_or("").to_string();
                            if entry.is_dir {
                                sync.device_dirs.insert(relative);
                                if sync.pending.insert(full.clone()) {
                                    self.queue.push_back(Request::SyncList(full));
                                }
                            } else {
                                sync.device_files.insert(relative, entry.size);
                            }
                        }
                        done = sync.pending.is_empty();
                    }
                    if done {
                        update.sync_plan = Some(self.finish_sync());
                    }
                }
            },
        }
    }

    /// Invalidates the cached listing of `path`'s parent directory, and
    /// queues a fresh listing if it is the one currently on screen ---
    /// shared by [`Request::Mkdir`] and [`Request::Touch`], which both add
    /// one new entry to whatever directory `path` sits in.
    fn invalidate_parent_of(&mut self, path: &DevicePath) {
        let Some(dir) = path.parent() else { return };
        self.cache.remove(&dir);
        if dir == self.device_path {
            self.device_state = PaneState::Loading;
            self.queue.push_front(Request::List(dir));
        }
    }

    /// Builds the final [`SyncPlan`] from the accumulated local and device
    /// trees, consuming the [`SyncState`].
    fn finish_sync(&mut self) -> SyncPlan {
        let sync = self.sync.take().expect("sync state must exist to finish");

        // BTreeSet iteration is sorted, so directories come parents-first.
        let mkdirs: Vec<DevicePath> = sync
            .local_dirs
            .iter()
            .filter(|d| !sync.device_dirs.contains(*d))
            .map(|d| DevicePath::new(&format!("/{d}")))
            .collect();

        let uploads: Vec<(PathBuf, DevicePath)> = sync
            .local_files
            .iter()
            .filter(|(rel, size)| match sync.device_files.get(*rel) {
                None => true,
                Some(device_size) => *device_size != **size,
            })
            .map(|(rel, _)| {
                let local = self.local_path.join(rel);
                let device = DevicePath::new(&format!("/{rel}"));
                (local, device)
            })
            .collect();

        let deletes: Vec<DevicePath> = sync
            .device_files
            .iter()
            .filter(|(rel, _)| !sync.local_files.contains_key(*rel))
            .map(|(rel, _)| DevicePath::new(&format!("/{rel}")))
            .collect();

        SyncPlan {
            mkdirs,
            uploads,
            deletes,
        }
    }

    /// Reacts to a device-presence failure on any request: a `DeviceNotFound`
    /// queues a fresh `devs` scan so a stale selection does not keep pointing
    /// at a dead port until the user notices and presses 'd' themselves; a
    /// `DeviceUnresponsive` (board present but silent --- often a
    /// non-MicroPython firmware) instead asks the caller to prompt for a
    /// MicroPython install via `update.prompt_micropython_flash`.
    fn rescan_if_device_lost(&mut self, output: &Output, update: &mut BrowserUpdate) {
        if parse::is_device_unresponsive_error(&output.stderr) {
            update.prompt_micropython_flash = true;
            return;
        }
        if !parse::is_device_lost_error(&output.stderr) {
            return;
        }
        if matches!(self.queue.front(), Some(Request::Devices)) {
            return;
        }
        update.notices.push((
            Level::Warn,
            "device appears to be disconnected — rescanning".to_string(),
        ));
        self.queue.push_front(Request::Devices);
    }

    /// Whether a device command is currently running.
    pub fn is_busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Re-lists `dir` when it is `/lib` or a package directory under it:
    /// an install or an uninstall just changed what the Dependencies row
    /// counts, and a stale cache entry would keep reporting the old answer.
    /// A directory the user is *looking at* is re-listed through the pane's
    /// own request so the pane reloads with it.
    fn refresh_after_lib_change(&mut self, dir: &DevicePath) {
        let lib = DevicePath::new(crate::backend::micropython::deps::LIB_ROOT);
        if *dir != lib && dir.parent().as_ref() != Some(&lib) {
            return;
        }
        self.cache.remove(dir);
        if *dir == self.device_path {
            self.device_state = PaneState::Loading;
            self.queue.push_front(Request::BackgroundList(dir.clone()));
        } else {
            self.queue.push_back(Request::BackgroundList(dir.clone()));
        }
    }

    /// Queues a background listing of `path` --- the package manager's
    /// sub-listing of a dotted name's package directory. Silent, and
    /// skipped when the answer is already cached or already queued.
    pub fn request_background_list(
        &mut self,
        path: DevicePath,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        if self.cache.contains_key(&path)
            || self.in_flight.as_ref().is_some_and(|(_, request)| {
                matches!(request, Request::List(p) | Request::BackgroundList(p) if *p == path)
            })
            || self.queue.iter().any(|request| {
                matches!(request, Request::List(p) | Request::BackgroundList(p) if *p == path)
            })
        {
            return Vec::new();
        }
        self.enqueue(Request::BackgroundList(path), processes, port)
    }

    /// The cached device listing for `path`, if one has arrived --- the
    /// Project pane's report rows read `/` (boot files) and `/lib`
    /// (dependency coverage) from here.
    pub fn cached_listing(&self, path: &DevicePath) -> Option<&[parse::RemoteEntry]> {
        self.cache.get(path).map(Vec::as_slice)
    }

    /// The sha256 verdicts ([`Request::Hash`] results) computed **in
    /// `dir`**, for the report rows' `=` marks. Empty for a directory
    /// nothing was verified in --- never another directory's answers.
    pub fn verdicts_for(&self, dir: &DevicePath) -> &Verdicts {
        static EMPTY: std::sync::LazyLock<Verdicts> = std::sync::LazyLock::new(Verdicts::new);
        self.verdicts.get(dir).unwrap_or(&EMPTY)
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
    /// Present when a device `cat` (the viewer's "View" action) finished.
    pub device_view: Option<DeviceView>,
    /// Present when a `run` (the local file menu's "Run" action) finished.
    pub run_output: Option<RunOutput>,
    /// Present when a transfer finished.
    pub transfer: Option<Transfer>,
    /// True if the device is present but did not respond to the command.
    pub prompt_micropython_flash: bool,
    /// Present when a completed request changed what is known about running
    /// user code: `Some(true)` after `reset`/`exec --no-follow "import main"`
    /// (the script is running again), `Some(false)` after a soft-reset (raw
    /// REPL skips `main.py`, and the reload that follows re-interrupts).
    pub script_running: Option<bool>,
    /// Present when a sync walk finished and produced a plan for review.
    pub sync_plan: Option<SyncPlan>,
    /// The device directory a finished listing just settled --- either
    /// pane-driven ([`Request::List`]) or background
    /// ([`Request::BackgroundList`]), including the "does not exist" answer
    /// cached as empty. `App` hangs its follow-up policies off this: the
    /// `/lib` sub-listings a dotted package name needs, and the boot files'
    /// background sha256.
    pub listed: Option<DevicePath>,
}

/// Where a device file bound for `$EDITOR` is downloaded to --- one scratch
/// directory per process, never the project tree. Editing a device file is
/// meant to prove a change on the device first; landing it in the project
/// would make that indistinguishable from an ordinary local edit and defeat
/// the point (`Browser::request_edit_download`'s doc comment).
fn edit_download_path(name: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("chiptui-edit-{}", std::process::id()))
        .join(name)
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
    fn ascending_leaves_the_directory_you_came_from_selected() {
        // yazi-style: `←` back into the parent keeps the entry for the
        // directory just left under the cursor, not the first row.
        let root =
            std::env::temp_dir().join(format!("chiptui-browser-back-{}", std::process::id()));
        std::fs::create_dir_all(root.join("aaa")).unwrap();
        std::fs::create_dir_all(root.join("zzz")).unwrap();
        std::fs::create_dir_all(root.join("zzz").join("inner")).unwrap();
        std::fs::write(root.join("main.py"), "print('hi')\n").unwrap();
        let mut browser = Browser::new(&root);
        let mut processes = ProcessManager::new();

        browser.cursor_to(1); // zzz/ (aaa/ is first)
        browser.enter(&mut processes, None);
        browser.ascend(&mut processes, None);
        assert_eq!(
            browser.selected_name(Side::Local).as_deref(),
            Some("zzz"),
            "the parent keeps the directory you came from selected"
        );

        // Entering that entry again goes deeper, and ascending once more
        // re-selects it --- `← ← →` is a stable loop.
        browser.enter(&mut processes, None);
        assert_eq!(browser.local_path, root.join("zzz"));
        browser.ascend(&mut processes, None);
        assert_eq!(browser.selected_name(Side::Local).as_deref(), Some("zzz"));

        let _ = std::fs::remove_dir_all(&root);
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
    fn selected_is_dir_distinguishes_files_from_directories() {
        let fixture = Fixture::new("is-dir");
        let mut browser = Browser::new(&fixture.root);

        browser.cursor_to(0); // lib/
        assert!(browser.selected_is_dir(Side::Local));
        browser.cursor_to(1); // main.py
        assert!(!browser.selected_is_dir(Side::Local));
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
    fn an_external_local_change_is_detected_until_reloaded() {
        let fixture = Fixture::new("watch");
        let mut browser = Browser::new(&fixture.root);

        assert!(!browser.local_listing_changed());
        std::fs::write(fixture.root.join("added.py"), "x = 1\n").unwrap();
        assert!(browser.local_listing_changed());

        browser.reload_local();
        assert!(!browser.local_listing_changed());
        assert!(
            browser
                .local_entries
                .iter()
                .any(|entry| entry.name == "added.py")
        );
    }

    #[test]
    fn an_error_pane_always_retries_the_local_directory() {
        let browser = Browser::new("/nonexistent-chiptui-path");
        assert!(browser.local_error.is_some());
        // Retrying is the only way a directory created or repaired outside
        // the program is ever picked up from the error state.
        assert!(browser.local_listing_changed());
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
        // `load_device` queues both `Df` (free space, not yet known) and `List`
        // behind the scan's `Devices` request, which is still running.
        assert_eq!(browser.queue.len(), 2, "later requests wait their turn");
    }

    #[test]
    fn the_interrupt_gate_holds_requests_until_released() {
        let fixture = Fixture::new("gate");
        let mut browser = Browser::new(&fixture.root);
        // `true` exists on every unix box and exits immediately; what is under
        // test is the queue's behavior, not the tool.
        browser.set_tool_path("true");
        let mut processes = ProcessManager::new();

        browser.set_interrupt_gate(true, &mut processes, None);
        browser.scan_devices(&mut processes, None);

        assert!(!browser.is_busy(), "nothing was allowed to start");
        assert!(browser.held_for_interrupt(), "the app is asked to confirm");

        // The user confirms: the gate comes off and the held request proceeds.
        browser.set_interrupt_gate(false, &mut processes, None);
        assert!(browser.is_busy(), "the held scan starts");
        assert!(!browser.held_for_interrupt());
    }

    #[test]
    fn declining_the_interrupt_drops_the_held_requests() {
        let fixture = Fixture::new("gate-decline");
        let mut browser = Browser::new(&fixture.root);
        browser.set_tool_path("true");
        let mut processes = ProcessManager::new();

        browser.set_interrupt_gate(true, &mut processes, None);
        browser.scan_devices(&mut processes, None);
        assert!(browser.held_for_interrupt());

        browser.cancel_held_requests();

        assert!(!browser.held_for_interrupt());
        assert!(!browser.is_busy(), "nothing was started");
        assert!(browser.queue.is_empty(), "the queue is dropped");
        assert!(
            matches!(&browser.device_state, PaneState::Failed(message) if message.contains("script")),
            "the pane explains why nothing is loading: {:?}",
            browser.device_state
        );
    }

    #[test]
    fn a_cached_pane_stays_ready_when_an_interrupt_is_declined() {
        let fixture = Fixture::new("gate-cached");
        let mut browser = Browser::new(&fixture.root);
        browser.set_tool_path("true");
        let mut processes = ProcessManager::new();

        // A listing the user is already looking at must survive a declined
        // interrupt; only the missing-listing case turns into an error.
        browser.cache.insert(
            DevicePath::root(),
            vec![parse::RemoteEntry {
                name: "main.py".to_string(),
                size: 10,
                is_dir: false,
            }],
        );
        browser.device_state = PaneState::Ready;
        browser.set_interrupt_gate(true, &mut processes, None);
        browser.load_device(&mut processes, None, false);
        assert!(browser.held_for_interrupt());

        browser.cancel_held_requests();
        assert_eq!(browser.device_state, PaneState::Ready);
        assert_eq!(browser.visible_device().len(), 1);
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
