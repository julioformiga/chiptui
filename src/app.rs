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
//! [`build_view`] (the build panel's list and commands),
//! [`flash_view`] (the `esptool` menu and online firmware search), and
//! [`overlay`] (modal key dispatch, including the shared confirm-dialog
//! machinery). They are `impl App` blocks split by file, not separate
//! types --- cross-submodule calls use `pub(super)`, so nothing outside
//! `app` gains access that the flat file did not already expose.

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use time::OffsetDateTime;

use crate::backend::zephyr::workspace::Workspace;
use crate::backend::{BackendKind, Capabilities, Capability};
use crate::browser::{Browser, Side};
use crate::console::{ConsoleLine, LineConsole};
use crate::device::{DevicePath, DeviceState};
use crate::event::AppEvent;
use crate::files::SyncStatus;
use crate::flash::FlashPanel;
use crate::logs::LogStore;
use crate::process::{ProcessId, ProcessManager};
use crate::project::{DetectionOutcome, DetectionSource, ProjectManager};

pub mod build_view;
pub mod devices;
pub mod file_browser;
pub mod flash_view;
pub mod help;
pub mod overlay;
pub mod probe;
pub mod project_view;
pub mod workspace_view;

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
/// The Project pane (row 1) holds the checklist the environment questions
/// moved into, so it *is* navigable --- but it deliberately stays off the
/// `Tab` tour (the tour walks the working panes; the questions are a
/// detour): `ctrl+p` enters it (and toggles back out to wherever focus
/// was), `Tab` leaves it onto the tour's first stop. `FilesLocal`/
/// `FilesDevice` are the dashboard's two file-browser columns; each is its
/// own stop so `Tab` walks all columns in one consistent tour instead of a
/// separate sub-focus inside the files row. `Build` is the build panel a
/// backend without a device filesystem shows (`SPEC.md` §10), and
/// `Workspace` the project-files pane beside it (the backend's shared
/// workspace, not the project).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Project,
    FilesLocal,
    FilesDevice,
    Workspace,
    Build,
    Logs,
}

/// One navigable row of the Project pane (row 1): whichever environment
/// question the selected backend asks there. The Zephyr rows mirror
/// [`crate::workspace::WorkspaceAction`] one-to-one (they run through it);
/// the MicroPython rows are their own state ([`App::mpy_projects`] and
/// friends), the last two plain reports no key answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRow {
    /// "Where is the Zephyr installation?" (the picker validates and
    /// persists it).
    ZephyrPath,
    /// "Where are your Zephyr projects?"
    ProjectsBase,
    /// "What am I building?" (the build panel's root, session-only).
    ProjectPath,
    /// "For which target?" --- the board with its optional shield riding
    /// on the same line; `←`/`→` pick which half `Enter` acts on.
    BoardShield,
    /// "Where are your MicroPython projects?" (`[micropython] projects`).
    MpyProjectsBase,
    /// "Which project to browse?" (re-roots the local pane, session-only).
    MpyProjectPath,
    /// Dependency files present in the project root (`requirements.txt`/`manifest.py`).
    MpyDependencies,
    /// Whether the board is believed to be running user code right now
    /// ([`crate::device::ScriptState`]).
    MpyScript,
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

/// A modal layer drawn above the panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// The help overlay (`?` / F1): one window with both divisions of
    /// [`crate::app::help`] --- Navigation as plain rows, Commands as the
    /// select. `selected` is the cursor among the (filtered) command rows;
    /// `Enter` activates the row by replaying its key after the help
    /// closes. `filter`/`filtering` are the search state --- `/` starts
    /// typing (every printable char is filter text, `j`/`k` included, so
    /// `Esc` returns to the cursor first and closes on the second press),
    /// and the filter narrows both divisions: the dashboard alone lists
    /// twenty-eight rows, so search is the way through them.
    Help {
        filter: String,
        filtering: bool,
        selected: usize,
    },
    /// Manual backend selection (`AGENTS.md` §4: detection must be overridable).
    BackendPicker { selected: usize },
    /// Serial device selection (`SPEC.md` §8: never guess which board).
    DevicePicker { selected: usize },
    /// The color theme picker (`t`): `Auto` first, then every
    /// `ratatui_themes::ThemeName`, cursor starting on the active choice.
    /// Picking one applies immediately and persists to the user config's
    /// `[ui] theme` (`App::apply_theme_picker`); `Auto` follows the active
    /// backend (Zephyr: Catppuccin Mocha, MicroPython: Everforest) instead
    /// of naming a theme outright.
    ThemePicker { selected: usize },
    /// A destructive esptool action awaiting explicit confirmation
    /// (`SPEC.md` §15). `message` is the literal command about to run, never
    /// a paraphrase.
    Confirm { message: String, confirm: bool },
    /// Firmware file selection when more than one `.bin`/`.elf` was found in
    /// `firmware/`.
    FirmwarePicker { selected: usize },
    /// Empty or unrecognized project: asks which backend this directory is
    /// (`SPEC.md` §7). Unlike [`Overlay::BackendPicker`] this fires
    /// automatically, offers no "Automatic" row (detection already failed to
    /// conclude one), and persists the choice to `chiptui.toml`.
    ProjectSetup { selected: usize },
    /// A firmware download would overwrite a file already in `firmware/`;
    /// needs explicit confirmation before running (`SPEC.md` §15 applied to
    /// a filesystem write rather than a device operation).
    ConfirmDownloadOverwrite {
        url: String,
        dest: PathBuf,
        confirm: bool,
    },
    /// Ask for confirmation before uploading a file or directory.
    ConfirmUpload {
        name: String,
        is_dir: bool,
        confirm: bool,
    },
    /// A destructive build-panel action (`Clean`, `Flash`) awaiting explicit
    /// confirmation, showing the literal command (the same rule as the
    /// esptool confirms, `SPEC.md` §15). The message is rebuilt from the
    /// panel state at draw time rather than stored: board and build
    /// directory cannot change while the overlay is open, and this way the
    /// shown command is always the one that would run.
    ConfirmBuild {
        action: crate::build::BuildAction,
        confirm: bool,
    },
    /// The board picker: a filterable `west boards` list, fetched in the
    /// background the first time it opens. The boards themselves live in
    /// [`App::build`] ([`crate::build::BuildPanel::boards`]) like the
    /// viewer's content lives in `App::viewer` --- an overlay holds only
    /// what a keypress changes, so rebuilding it per key never re-clones
    /// the list. `input` is the filter text.
    BoardPicker { input: String, selected: usize },
    /// The shield picker: the same filterable list grammar over `west
    /// shields`, with a leading `(none)` row --- the shield is optional, and
    /// that row is how an existing pick gets cleared. The list itself lives
    /// in [`App::build`] ([`crate::build::BuildPanel::shields`]) like the
    /// boards do. `input` is the filter text.
    ShieldPicker { input: String, selected: usize },
    /// The installation-directory picker: a real filesystem browser (no
    /// discovery guesses --- the user knows where their Zephyr lives).
    /// `error` holds the validation message when an accepted directory
    /// turned out not to be an installation, including the install guide
    /// link; any navigation clears it. `purpose` is which question the
    /// picker answers (installation or projects folder) --- one navigation
    /// component, two validations.
    DirPicker {
        purpose: crate::workspace::DirPurpose,
        path: std::path::PathBuf,
        selected: usize,
        error: Option<String>,
    },
    /// The project picker: the configured projects folder's subdirectories.
    /// For Zephyr (`mpy: false`) each row carries whether it holds build
    /// elements, and accepting a non-buildable directory keeps the picker
    /// open with the reason (`error`) --- a project without a
    /// `CMakeLists.txt` is never built silently. For MicroPython (`mpy:`
    /// `true`) every subdirectory is a project (no build step), so nothing
    /// is marked and nothing is refused. The choice itself is session-only
    /// (`SPEC.md` §10) either way.
    ProjectPicker {
        mpy: bool,
        selected: usize,
        error: Option<String>,
    },
    /// The build-directory picker: the project's configured `build*`
    /// directories plus a typed new name (`west build -d`).
    BuildDirPicker { input: String, selected: usize },
    /// The entry under the cursor in the file browser (`enter`): a small
    /// menu of what to do with it. Which actions show up depends on the pane,
    /// on whether it is a directory, and --- for a file --- whether
    /// [`crate::files::is_text_like`] considers it text --- see
    /// [`FileAction::for_entry`]. The Zephyr workspace pane's embedded file
    /// list never opens this: its keys act directly (see
    /// [`App::run_file_action`]).
    FileActions {
        side: Side,
        name: String,
        is_dir: bool,
        /// The comparison verdict for `name` (`Browser::statuses`), snapshot
        /// when the menu opened so [`FileAction::for_entry`] can offer a
        /// [`FileAction::Diff`] only when the two sides are known to (or might)
        /// differ. `None` when the entry has no comparable status.
        status: Option<SyncStatus>,
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
    ConfirmRestartDevice { confirm: bool },
    /// Ask to flash MicroPython if device is unresponsive.
    ConfirmEraseForMicroPython { confirm: bool },
    /// Ask for confirmation before deleting a file or directory.
    ConfirmDelete {
        side: Side,
        name: String,
        is_dir: bool,
        confirm: bool,
    },
    /// Inline text entry for creating a new entry in `side`'s current
    /// directory (`a`). A trailing `/` on the typed name means "create a
    /// directory" (`SPEC.md` §9's "create directory" action); otherwise an
    /// empty file.
    CreateEntry { side: Side, input: String },
    /// Inline text entry for renaming the entry under the cursor (`r` in the
    /// workspace file list). `name` is the entry's current name, `input` the
    /// edit buffer, pre-filled with it --- editing starts from the end, and
    /// an unedited `Enter` is a no-op, not an error.
    RenameEntry { name: String, input: String },
    /// Inline text entry for `mip install` (`i` on the device pane). Unlike
    /// [`Overlay::CreateEntry`] this is not tied to `side` or a selected
    /// entry --- it acts on the device as a whole, not the file under the
    /// cursor.
    PackageInstall { input: String },
    /// A sync plan produced by [`Browser::request_sync`], awaiting the
    /// user's review before execution (`S` in the file browser). Default
    /// is No when the plan includes device-only file deletions, since
    /// deleting is destructive (`SPEC.md` §15).
    SyncPreview {
        plan: crate::browser::SyncPlan,
        confirm: bool,
    },
    /// Device requests are being held: the app believes a script is running
    /// on the device, and `mpremote` interrupts it (Ctrl-C, then raw REPL)
    /// for every device command --- including the one the user just asked
    /// for. Default is No, like every interruption-confirm here. Accepting
    /// resumes the held queue and arms the restore question for when it
    /// drains; declining drops the queue.
    ConfirmInterruptDevice { confirm: bool },
    /// Leaving this project for the home screen while commands are still
    /// running: they are cancelled with the session, so the count is named
    /// and the default is No, like every other confirm that loses work.
    ConfirmSwitchProject { confirm: bool },
    /// An interruption the user accepted has finished: how (or whether) to
    /// bring the stopped script back. A three-row picker rather than a
    /// Yes/No, because "restart" has two honest flavors with different
    /// tradeoffs (see [`Self::apply_restore_device_script`]).
    RestoreDeviceScript { selected: usize },
}

/// One action offered by [`Overlay::FileActions`] for the entry under the
/// cursor. The files pane is a sync tool, not a filesystem manager, so the
/// choices mirror that: move a copy across, or work with the copy already
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// Descends into a directory --- the menu's default entry for one, so a
    /// reflex `Enter` twice still just browses in, one extra keypress from
    /// what a bare `Enter` used to do before directories gained the rest of
    /// this menu too.
    Open,
    SendToDevice,
    Download,
    /// Runs a local file on the device without copying it in
    /// (`mpremote run`) --- only ever offered on [`Side::Local`], since the
    /// underlying command takes a host path. See [`Capability::Run`].
    Run,
    View,
    Edit,
    /// Shows a unified diff of the local copy against the device copy in the
    /// file viewer --- only offered when both sides exist as text and the
    /// comparison verdict says they differ (or might, same size unchecked).
    Diff,
    Delete,
}

impl FileAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "📂 Open",
            Self::SendToDevice => "📤 Send to device",
            Self::Download => "📥 Download",
            Self::Run => "▶  Run",
            Self::View => "👁  View",
            Self::Edit => "📝 Edit",
            Self::Diff => "🔀 Diff",
            Self::Delete => "🗑  Delete",
        }
    }

    /// The actions offered for the entry under the cursor, in menu order.
    ///
    /// A directory gets `Open` first (descend), plus `Delete` and, when the
    /// backend can upload, `SendToDevice` --- never `View`/`Edit`/`Diff`,
    /// which need file contents. A file never offers `Open`; `View`/`Edit`
    /// appear only when `is_text` ([`crate::files::is_text_like`]) --- a binary
    /// file (e.g. a `.mpy`) can still be sent, downloaded and deleted, just
    /// not previewed or opened in `$EDITOR`.
    ///
    /// The transfer actions are capability-gated like `Run`, not just hidden
    /// by this menu's judgement: a backend without [`Capability::Upload`]
    /// offers no `SendToDevice`, and `Diff` needs
    /// [`Capability::Filesystem`] --- there is no second copy to compare
    /// against without one --- offered when `status` marks the entry as
    /// differing or as same-size but unchecked, since that is exactly when a
    /// content diff adds information the size markers cannot.
    pub fn for_entry(
        side: Side,
        is_dir: bool,
        is_text: bool,
        status: Option<SyncStatus>,
        capabilities: Capabilities,
    ) -> Vec<FileAction> {
        if is_dir {
            let mut actions = vec![Self::Open];
            match side {
                Side::Local if capabilities.contains(Capability::Upload) => {
                    actions.push(Self::SendToDevice);
                }
                Side::Device => actions.push(Self::Download),
                Side::Local => {}
            }
            actions.push(Self::Delete);
            actions
        } else {
            let mut actions = match side {
                Side::Local if capabilities.contains(Capability::Upload) => {
                    vec![Self::SendToDevice]
                }
                Side::Device => vec![Self::Download],
                Side::Local => Vec::new(),
            };
            if is_text {
                if side == Side::Local && capabilities.contains(Capability::Run) {
                    actions.push(Self::Run);
                }
                actions.push(Self::View);
                actions.push(Self::Edit);
                if capabilities.contains(Capability::Filesystem)
                    && matches!(
                        status,
                        Some(SyncStatus::Differs) | Some(SyncStatus::SameSize)
                    )
                {
                    actions.push(Self::Diff);
                }
            }
            actions.push(Self::Delete);
            actions
        }
    }
}

/// A freshly opened help overlay: no filter, cursor on the first command.
pub const OVERLAY_HELP: Overlay = Overlay::Help {
    filter: String::new(),
    filtering: false,
    selected: 0,
};

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
            // Deliberately not `{path}.output` or anything else ending the
            // string in the script's own extension: `highlight::Language::
            // from_filename` keys off the text after the last '.', and this
            // is plain captured output, not Python source, so it must not
            // look like a `.py` file to it.
            ViewerSource::RunOutput(path) => format!("{} — output", path.display()),
            ViewerSource::Diff { local, .. } => {
                let name = local
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("Diff: {name}  (local ↔ device)")
            }
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
    /// Captured stdout of a local script run on the device
    /// (`FileAction::Run`), keyed by the local script's path.
    RunOutput(PathBuf),
    /// A unified diff of the local copy (`local`) against the device copy
    /// (`device`). Like [`Self::Device`], the device half arrives
    /// asynchronously via a `cat`: the viewer opens in
    /// [`ViewerState::Loading`] and [`App::apply_device_view`] computes the
    /// diff once the device content lands.
    Diff {
        local: PathBuf,
        device: DevicePath,
    },
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

/// State of a script run displayed in the Monitor tab under
/// [`MonitorSource::Run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunState {
    /// No run has been started.
    #[default]
    Idle,
    /// Process is running.
    Running,
    /// Process finished (success, failure, timeout, or cancellation).
    Finished,
}

/// One line of streamed run output with its wall-clock timestamp.
#[derive(Debug, Clone)]
pub struct RunLine {
    pub timestamp: OffsetDateTime,
    pub text: String,
}

impl ConsoleLine for RunLine {
    fn blank() -> Self {
        Self {
            timestamp: OffsetDateTime::now_utc(),
            text: String::new(),
        }
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn text_mut(&mut self) -> &mut String {
        &mut self.text
    }
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

/// One row of the theme picker. Every `Named` row is a fixed theme that
/// applies to all projects alike; `Auto` is the one answer that depends on
/// the session --- it follows the active backend, so a Zephyr project
/// renders in Catppuccin Mocha and a MicroPython one in Everforest, with
/// Tokyo Night standing in wherever no backend is active yet (the home
/// screen, an unresolved project).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeChoice {
    Auto,
    Named(ratatui_themes::ThemeName),
}

impl ThemeChoice {
    /// The picker's rows: `Auto` first, then every fixed theme --- the same
    /// "the meta answer leads" order the backend picker follows.
    pub fn all() -> Vec<Self> {
        std::iter::once(Self::Auto)
            .chain(
                ratatui_themes::ThemeName::all()
                    .iter()
                    .copied()
                    .map(Self::Named),
            )
            .collect()
    }

    /// Parses a stored `[ui] theme` slug: `auto` is ours, every other slug
    /// belongs to `ratatui_themes`.
    pub fn from_slug(slug: &str) -> Option<Self> {
        if slug.eq_ignore_ascii_case("auto") {
            Some(Self::Auto)
        } else {
            slug.parse().ok().map(Self::Named)
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Named(theme) => theme.slug(),
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Named(theme) => theme.display_name(),
        }
    }

    /// The concrete theme this choice renders as for the active backend.
    pub fn resolve(self, backend: Option<BackendKind>) -> ratatui_themes::ThemeName {
        match (self, backend) {
            (Self::Auto, Some(BackendKind::Zephyr)) => ratatui_themes::ThemeName::CatppuccinMocha,
            (Self::Auto, Some(BackendKind::MicroPython)) => ratatui_themes::ThemeName::Everforest,
            (Self::Auto, None) => ratatui_themes::ThemeName::TokyoNight,
            (Self::Named(theme), _) => theme,
        }
    }
}

/// Resolves the stored `[ui] theme` choice, falling back to Tokyo Night on
/// an absent or unparsable slug --- shared by [`App::new`] and `main.rs`'s
/// home screen, which draws before an `App` exists at all.
pub fn resolve_theme(config_dir: &std::path::Path) -> ThemeChoice {
    crate::settings::theme(config_dir)
        .and_then(|slug| ThemeChoice::from_slug(&slug))
        .unwrap_or(ThemeChoice::Named(ratatui_themes::ThemeName::TokyoNight))
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
    /// The Monitor tab's scroll position, reset whenever the source changes
    /// (a new feed starts at its tail).
    pub monitor_scroll: MonitorScroll,
    /// The Monitor tab's on-screen geometry, published by the renderer.
    pub monitor_view: MonitorView,
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
    /// The build panel, created once for a backend that can build and has no
    /// device filesystem to browse instead (right half of row 2).
    pub build: Option<crate::build::BuildPanel>,
    /// The workspace pane, created beside the build panel for a backend
    /// that maintains a shared environment ([`crate::backend::Capability::
    /// WorkspaceSync`]): which west workspace/venv/SDK the commands run
    /// against, plus `west update` and `west sdk list`.
    pub workspace: Option<crate::workspace::WorkspacePanel>,
    /// The Project pane's cursor, over its checklist rows (the environment
    /// questions that moved out of the workspace pane into row 1).
    pub project_cursor: usize,
    /// Which half of the merged `Board · Shield` row the keys act on:
    /// `true` the board (the left, required half), `false` the shield.
    /// Switched by `←`/`→` while the row is selected.
    pub board_segment: bool,
    /// Where `ctrl+p` returns to when it toggles the Project pane's focus
    /// away --- the pane is a detour off the `Tab` tour, and the way back
    /// is where the detour started.
    focus_before_project: Option<Focus>,
    /// The MicroPython projects folder (`[micropython] projects`, user
    /// config) --- the same question Zephyr's `[zephyr] projects` answers,
    /// resolved once per session.
    pub mpy_projects: Option<PathBuf>,
    /// Why a configured MicroPython projects folder failed validation.
    pub mpy_projects_invalid: Option<String>,
    /// The picked MicroPython project: session-only, re-rooting the file
    /// browser's local pane (nothing written --- the folder is the persisted
    /// half of the answer, the project is not).
    pub mpy_root: Option<PathBuf>,
    /// Whether `[micropython] projects` was read this session (the answer
    /// is refreshed by the pickers, not re-read per frame).
    mpy_projects_loaded: bool,
    /// The connected board's MicroPython version, read off the REPL banner
    /// the probe (or the monitor) already sees, and dropped with the board
    /// when it disconnects.
    pub mpy_version: Option<String>,
    /// Set by the build panel's `menuconfig` action; consumed by the binary's
    /// event loop, which owns the terminal handle needed to suspend the
    /// alternate screen for the interactive child --- the same hand-off as
    /// [`Self::pending_edit`].
    pending_command: Option<crate::process::Command>,
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
    /// VT interpretation of the monitor's raw output (cursor position and
    /// escape-sequence state) --- the REPL echoes redraw sequences that must
    /// not reach the rendered lines as text.
    monitor_console: LineConsole,
    pending_monitor: Option<PendingMonitor>,

    /// Active or last-finished `mpremote run` session, shown in the Monitor
    /// tab under [`MonitorSource::Run`]. Spawned in a PTY so Ctrl+C can
    /// interrupt the script on the device.
    pub run_process: Option<ProcessId>,
    pub run_output: Vec<RunLine>,
    /// VT interpretation of the run session's raw output, mirroring
    /// [`Self::monitor_console`].
    run_console: LineConsole,
    pub run_script: Option<PathBuf>,
    pub run_state: RunState,
    /// Set by [`App::defer_device_info_query`] when a device is newly
    /// selected while `mpremote` is (or is about to be) busy; consumed the
    /// moment [`Self::browser`] next goes idle. `esptool` and `mpremote`
    /// hold the serial port exclusively, so the background chip query
    /// must never race the file listing that a fresh device selection also
    /// kicks off (`AGENTS.md` §5's "one tool at a time" applies across
    /// tools too, not just within `mpremote`).
    flash_query_pending: bool,
    /// The first device listing is being held behind the identification
    /// chain on a newly selected device --- first the background
    /// `esptool chip-id`, then the firmware read its success arms
    /// ([`flash_view`]); [`Self::drive_held_root_listing`] re-evaluates it
    /// each time a link of that chain reports back.
    held_root_listing: bool,
    /// The port the firmware-identification read covers (pending, running
    /// or concluded): the verdict belongs to the board it was read from,
    /// so the first listing is gated against the selected port, and a
    /// device switch or re-flash drops the answer. See [`flash_view`].
    firmware_check_port: Option<String>,
    /// Where [`Self::firmware_check_port`]'s identification read stands:
    /// `Pending` waits for a free port (polled from the tick), `Running`
    /// holds the first listing behind the read, and `Idle` means concluded
    /// --- a verdict in [`Self::flash`], or a read that could not produce
    /// one (the listing then proceeds ungated and lets mpremote speak for
    /// itself).
    firmware_check: flash_view::FirmwareCheck,
    /// A short-lived `mpremote repl` session asking a newly selected device
    /// whether a script is running, *before* the first filesystem operation
    /// interrupts it --- see [`probe`]. `None` whenever no probe is in flight.
    probe: Option<probe::DeviceProbe>,
    /// The port the current (or last) probe covered; the probe runs once per
    /// selection, not before every command.
    probed_port: Option<String>,
    /// Set when the user accepts interrupting a running script: once the
    /// interrupted operations drain, [`Overlay::RestoreDeviceScript`] asks
    /// how to bring the script back.
    restore_pending: bool,
    /// Where [`Self::scan_serial_devices`] looks for USB serial ports.
    /// `/dev` in real use; tests point it at a fixture directory so the
    /// scan is deterministic regardless of what is plugged into the
    /// machine running them.
    serial_dir: std::path::PathBuf,
    /// Where `~` in configuration resolves (`$HOME` in real use; tests
    /// point it at a fixture directory so workspace discovery stays
    /// deterministic regardless of the machine).
    home_dir: std::path::PathBuf,
    /// Where the user config lives, resolved once from the environment
    /// ([`crate::settings::default_config_dir`]) and moved with `home_dir`
    /// afterwards --- so redirecting the home really does redirect every
    /// config read, `$XDG_CONFIG_HOME` included.
    config_dir: std::path::PathBuf,
    /// The stored theme choice --- `Auto` unless `[ui] theme` in the user
    /// config names a fixed theme. The concrete theme rendered each frame
    /// is derived from it plus the active backend ([`Self::theme`]), so an
    /// `Auto` session recolors itself when the backend changes. Loaded once
    /// at startup and cached: unlike
    /// [`ZephyrSettings`](crate::settings::ZephyrSettings) (recomputed on
    /// demand), this is read every frame by the renderer.
    theme: ThemeChoice,
    /// Memoized [`Self::tool_status`]. The render path asks twice a frame
    /// (once to measure the pane, once to draw it) for an answer that only
    /// changes with the selected backend or the resolved tool locations, and
    /// computing it walks every `PATH` entry per tool. Keyed on those two
    /// inputs rather than invalidated by hand, so a backend override applied
    /// straight to `manager` --- as tests do --- still recomputes.
    tool_status_cache: std::cell::RefCell<Option<ToolStatusMemo>>,
    /// The user asked for the home screen (`P`); the binary's loop reads it
    /// and swaps screens.
    switch_requested: bool,
    should_quit: bool,
    last_port_count: Option<usize>,
}

/// A tool report with the two inputs it was computed from --- see
/// [`App::tool_status`].
struct ToolStatusMemo {
    kind: BackendKind,
    located: Vec<(&'static str, std::path::PathBuf)>,
    status: Vec<(&'static str, bool)>,
}

impl App {
    pub fn new(start_dir: impl Into<PathBuf>) -> Self {
        let home =
            std::env::var_os("HOME").map_or_else(std::path::PathBuf::new, std::path::PathBuf::from);
        let config_dir = crate::settings::default_config_dir(&home);
        let theme = resolve_theme(&config_dir);
        let mut manager = ProjectManager::new(start_dir);
        manager.set_known_projects(crate::settings::ProjectRegistry::load(&config_dir, &home));
        Self {
            manager,
            logs: LogStore::default(),
            view: View::Dashboard,
            focus: Focus::FilesLocal,
            log_tab: LogTab::default(),
            monitor_source: MonitorSource::default(),
            monitor_scroll: MonitorScroll::default(),
            monitor_view: MonitorView::default(),
            overlay: None,
            log_viewport: 1,
            ticks: 0,
            processes: ProcessManager::new(),
            devices: DeviceState::new(),
            browser: None,
            flash: None,
            build: None,
            workspace: None,
            project_cursor: 0,
            board_segment: true,
            focus_before_project: None,
            mpy_projects: None,
            mpy_projects_invalid: None,
            mpy_root: None,
            mpy_projects_loaded: false,
            mpy_version: None,
            pending_command: None,
            viewer: None,
            viewer_viewport: 1,
            pending_edit: None,
            device_monitor_process: None,
            device_monitor_output: Vec::new(),
            monitor_console: LineConsole::new(),
            pending_monitor: None,
            run_process: None,
            run_output: Vec::new(),
            run_console: LineConsole::new(),
            run_script: None,
            run_state: RunState::default(),
            flash_query_pending: false,
            held_root_listing: false,
            firmware_check_port: None,
            firmware_check: flash_view::FirmwareCheck::Idle,
            probe: None,
            probed_port: None,
            restore_pending: false,
            serial_dir: std::path::PathBuf::from("/dev"),
            config_dir,
            theme,
            home_dir: home,
            tool_status_cache: std::cell::RefCell::new(None),
            switch_requested: false,
            should_quit: false,
            last_port_count: None,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// The palette actually rendered this frame --- deliberately named apart
    /// from [`crate::backend::BackendKind::palette`], which answers a
    /// different question ("which backend is this row?") and coexists with
    /// this one rather than being replaced by it. While [`Overlay::ThemePicker`]
    /// is open this previews the hovered row live (the whole UI behind the
    /// popup, popup included, so a pick can be judged before it commits);
    /// [`Self::theme`] itself is untouched until `Enter`, so `Esc` reverts
    /// for free --- nothing was ever committed to preview.
    pub fn theme_palette(&self) -> ratatui_themes::ThemePalette {
        self.previewed_theme().palette()
    }

    /// The theme this session renders in right now: the stored choice
    /// resolved against the active backend, so an `Auto` choice follows a
    /// backend override or re-detection live. Deliberately named apart
    /// from [`crate::backend::BackendKind::palette`], which answers a
    /// different question ("which backend is this row?") and coexists with
    /// this one rather than being replaced by it. While [`Overlay::ThemePicker`]
    /// is open the palette previews the hovered row live (the whole UI behind the
    /// popup, popup included, so a pick can be judged before it commits);
    /// this value itself is untouched until `Enter`, so `Esc` reverts for
    /// free --- nothing was ever committed to preview.
    pub fn theme(&self) -> ratatui_themes::ThemeName {
        self.theme.resolve(self.manager.selected_kind())
    }

    /// The stored choice behind [`Self::theme`] --- what the picker's
    /// "(active)" marker sits on and what a restart would reload: `Auto`
    /// when the session follows the backend, a fixed theme otherwise.
    pub fn theme_choice(&self) -> ThemeChoice {
        self.theme
    }

    fn previewed_theme(&self) -> ratatui_themes::ThemeName {
        match &self.overlay {
            Some(Overlay::ThemePicker { selected }) => ThemeChoice::all()
                .get(*selected)
                .copied()
                .map(|choice| choice.resolve(self.manager.selected_kind()))
                .unwrap_or_else(|| self.theme()),
            _ => self.theme(),
        }
    }

    /// Opens the theme picker (`t`) with the cursor on the currently active
    /// choice, the same "start where the current answer is" convention the
    /// backend override picker follows.
    fn open_theme_picker(&mut self) {
        let selected = ThemeChoice::all()
            .iter()
            .position(|&candidate| candidate == self.theme)
            .unwrap_or(0);
        self.overlay = Some(Overlay::ThemePicker { selected });
    }

    /// Applies the picked choice immediately (no restart needed --- the next
    /// frame just reads a different [`Self::theme_palette`]) and persists it
    /// to the user config the same way `workspace_view`'s
    /// `accept_workspace_dir` saves the workspace answer: a failed write
    /// still applies the theme for this session, it just cannot survive a
    /// restart, so it is logged as a warning rather than lost silently.
    fn apply_theme_picker(&mut self, selected: usize) {
        let Some(choice) = ThemeChoice::all().get(selected).copied() else {
            return;
        };
        self.theme = choice;
        let applied = match choice {
            ThemeChoice::Auto => {
                "Auto --- Zephyr: Catppuccin Mocha, MicroPython: Everforest".to_string()
            }
            ThemeChoice::Named(theme) => theme.display_name().to_string(),
        };
        let config = self.user_config_path();
        match crate::settings::save_theme(&config, choice.slug()) {
            Ok(()) => self
                .logs
                .info(format!("theme set to {applied} ({})", config.display())),
            Err(err) => self.logs.warn(format!(
                "theme set to {applied} for this session, but could not save it to {}: {err}",
                config.display()
            )),
        }
    }

    /// Where `~` in configuration resolves; also what the workspace pane
    /// names when it tells the user where the config file lives.
    pub fn home_dir(&self) -> &std::path::Path {
        &self.home_dir
    }

    /// The user config file this session reads and writes.
    pub fn user_config_path(&self) -> std::path::PathBuf {
        crate::settings::user_config_path(&self.config_dir)
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

                self.ensure_workspace_panel();
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
        if self.manager.override_kind().is_some()
            || matches!(
                detection.source,
                DetectionSource::Config | DetectionSource::Registered
            )
        {
            return;
        }
        if matches!(
            detection.outcome,
            DetectionOutcome::Unknown | DetectionOutcome::Ambiguous(_)
        ) {
            self.overlay = Some(Overlay::ProjectSetup { selected: 0 });
        }
    }

    /// Records the open project in the user config's registry (`SPEC.md`
    /// §7), stamping it as the most recently opened.
    ///
    /// This is the single place a project becomes "known": every way of
    /// arriving at a dashboard --- a `chiptui.toml` in the tree, evidence
    /// alone, the empty-project prompt, a project just created on the home
    /// screen --- passes through here, which is what keeps the home screen's
    /// list complete without any of them writing into the project directory.
    /// A directory whose backend is still unknown is not recorded; there
    /// would be nothing to record about it.
    pub fn record_open_project(&mut self) {
        let (Some(kind), Some(root)) = (self.manager.selected_kind(), self.manager.root()) else {
            return;
        };
        let root = root.to_path_buf();
        let mut entry = crate::settings::ProjectEntry::new(&root, kind).opened_now();
        // A name the user edited into the config is theirs, not ours to
        // reset on every open --- and so are the saved board/shield answers:
        // recording the open must not be what forgets them.
        if let Some(known) = self.manager.known_projects().entry_for(&root) {
            entry.name = known.name.clone();
            entry.board = known.board.clone();
            entry.shield = known.shield.clone();
        }

        let config = self.user_config_path();
        if let Err(err) = crate::settings::record_project(&config, entry) {
            self.logs.warn(format!(
                "could not record this project in {}: {err}",
                config.display()
            ));
            return;
        }
        self.manager
            .set_known_projects(crate::settings::ProjectRegistry::load(
                &self.config_dir,
                &self.home_dir,
            ));
    }

    /// `P`: back to the home screen to open another project. Leaving means
    /// dropping this project's session, so anything still running is named
    /// in a confirmation first (`SPEC.md` §15's rule applied to losing work
    /// rather than to the device) --- with nothing running there is nothing
    /// to warn about, and the screen simply changes.
    pub fn request_home_screen(&mut self) {
        if self.running_commands() == 0 {
            self.request_project_switch();
            return;
        }
        self.overlay = Some(Overlay::ConfirmSwitchProject { confirm: false });
    }

    /// External commands this session would lose by leaving it --- builds,
    /// device operations and the PTY sessions alike, since they all run
    /// through the one [`crate::process::ProcessManager`].
    pub fn running_commands(&self) -> usize {
        self.processes.running_count()
    }

    /// Asks the binary to close this project and go back to the home screen.
    /// The App is dropped by the caller, which is what cancels every running
    /// command and releases the serial port --- see
    /// [`crate::process::ProcessManager`]'s `Drop`.
    pub fn request_project_switch(&mut self) {
        self.switch_requested = true;
    }

    /// Whether the event loop should give the terminal back so the home
    /// screen can take over --- the same standing as [`Self::should_quit`].
    pub fn switch_requested(&self) -> bool {
        self.switch_requested
    }

    /// Consumed by the binary's loop, like [`Self::take_pending_command`].
    pub fn take_switch_request(&mut self) -> bool {
        std::mem::take(&mut self.switch_requested)
    }

    /// Warns about required tools that cannot be run.
    ///
    /// Judging a Zephyr `west` against the inherited `PATH` before the
    /// workspace venv holding it is known would be a false alarm, so every
    /// call site resolves the workspace first --- see
    /// [`Self::ensure_workspace_panel`].
    fn report_tools(&mut self) {
        let Some(kind) = self.manager.selected_kind() else {
            return;
        };
        let missing: Vec<&str> = self
            .tool_status()
            .into_iter()
            .filter(|(_, available)| !*available)
            .map(|(tool, _)| tool)
            .collect();

        if !missing.is_empty() {
            self.logs.warn(format!(
                "{kind}: {} not found --- install it to enable the related operations",
                missing.join(", ")
            ));
        }
    }

    /// The selected backend's required tools with whether each is actually
    /// runnable --- the one definition, shared by [`Self::report_tools`]'s
    /// warning and the Project pane's tools row.
    ///
    /// The only thing added here is the *answer* to "did anything resolve a
    /// location for one of these tools": a resolved workspace names the
    /// tools it owns ([`crate::backend::zephyr::workspace::Workspace::tool_locations`])
    /// and the registry judges those files instead of `PATH`. Which tool
    /// that happens to be is the workspace's business, never a branch here.
    ///
    /// Memoized on those two inputs: the render path asks twice a frame and
    /// a miss costs a `PATH` walk per unlocated tool.
    pub fn tool_status(&self) -> Vec<(&'static str, bool)> {
        let Some(kind) = self.manager.selected_kind() else {
            return Vec::new();
        };
        let located = self
            .workspace
            .as_ref()
            .and_then(|panel| panel.resolved.as_ref())
            .map(Workspace::tool_locations)
            .unwrap_or_default();

        let mut cache = self.tool_status_cache.borrow_mut();
        if let Some(memo) = cache.as_ref()
            && memo.kind == kind
            && memo.located == located
        {
            return memo.status.clone();
        }

        let status = self.manager.registry().tool_status(kind, &located);
        *cache = Some(ToolStatusMemo {
            kind,
            located,
            status: status.clone(),
        });
        status
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
                self.tick_probe();
                self.check_interrupt_gate();
                self.maybe_offer_restore();
                self.maybe_run_deferred_firmware_check();
                self.maybe_run_deferred_version_hunt();
                if self.maybe_run_deferred_flash_query() == flash_view::DeferredQuery::Dropped {
                    // The held listing was waiting on a query that can never
                    // start now; listing beats waiting forever.
                    self.drive_held_root_listing();
                }
                self.drive_held_root_listing();
            }
            AppEvent::Process(event) => self.on_process(&event),
        }
    }

    fn check_device_hotplug(&mut self) {
        // Every device-shaped backend watches for connect/disconnect: one
        // with a filesystem rescans through `mpremote devs`, a monitor-only
        // one (Zephyr) through the same `/dev` walk its scan is. A backend
        // with neither has no device status worth keeping fresh.
        let caps = self.manager.capabilities();
        if !caps.contains(Capability::Filesystem) && !caps.contains(Capability::Monitor) {
            return;
        }
        // only check every 4 ticks (1 second)
        if !self.ticks.is_multiple_of(4) {
            return;
        }
        // A rescan can open the device picker (several boards appeared) or
        // clear the selection (they all left); neither belongs on top of a
        // dialog the user is mid-answer on. The count comparison simply
        // happens on a later tick once it closes.
        if self.overlay.is_some() {
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
        // A probe holds the port exactly like a listing does, and a scan
        // racing it would ask the same serial port two questions at once.
        if self.probe.is_some() {
            return;
        }
        if self.device_monitor_process.is_some() {
            return;
        }
        if self.run_process.is_some() {
            return;
        }

        let current_count = crate::device::usb_serial_ports(&self.serial_dir).len();
        if let Some(last) = self.last_port_count
            && current_count != last
        {
            self.logs
                .info("device connection change detected, rescanning...");
            if caps.contains(Capability::Filesystem) {
                self.scan_devices();
            } else {
                self.scan_serial_devices();
            }
        }
        self.last_port_count = Some(current_count);
    }

    /// Routes a process result to whatever asked for it.
    ///
    /// Both subsystems see every event: each guards on its own in-flight
    /// process id and is a no-op for an event it did not start, which is
    /// simpler than tracking ownership here.
    fn on_process(&mut self, event: &crate::process::ProcessEvent) {
        match event {
            // The script-probe session (PTY): raw output arrives as bytes, and
            // its exit is what releases the port for the listing it deferred.
            crate::process::ProcessEvent::Output { id, text }
                if self
                    .probe
                    .as_ref()
                    .is_some_and(|probe| probe.process == *id) =>
            {
                self.on_probe_output(text);
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome: _,
                duration: _,
            } if self
                .probe
                .as_ref()
                .is_some_and(|probe| probe.process == *id) =>
            {
                self.finish_probe();
                return;
            }
            crate::process::ProcessEvent::Line {
                id,
                stream: _,
                text,
            } if Some(*id) == self.device_monitor_process => {
                self.monitor_console
                    .push_line(&mut self.device_monitor_output, text.clone());
                self.update_script_from_monitor();
                return;
            }
            crate::process::ProcessEvent::Output { id, text }
                if Some(*id) == self.device_monitor_process =>
            {
                self.monitor_console
                    .feed(&mut self.device_monitor_output, text);
                self.update_script_from_monitor();
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome,
                duration: _,
            } if Some(*id) == self.device_monitor_process => {
                self.device_monitor_process = None;
                self.monitor_console.push_line(
                    &mut self.device_monitor_output,
                    format!("[monitor {}]", outcome.summary()),
                );
                return;
            }
            // Run session (PTY): streamed output arrives as raw bytes.
            crate::process::ProcessEvent::Output { id, text } if Some(*id) == self.run_process => {
                self.run_console.feed(&mut self.run_output, text);
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome,
                duration: _,
            } if Some(*id) == self.run_process => {
                self.run_process = None;
                self.run_state = RunState::Finished;
                self.run_console
                    .push_line(&mut self.run_output, format!("[run {}]", outcome.summary()));
                // The run *was* the user code: finished means the device sits
                // at its REPL with nothing executing.
                self.set_script_state(crate::device::ScriptState::Stopped);
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
                        self.device_disconnected();
                    } else if self.devices.needs_selection() {
                        // Several boards: ask before touching any of them.
                        self.open_device_picker();
                    } else {
                        // Exactly one, or a previous choice still present.
                        // The chip query is deferred *before* the listing
                        // is requested: `load_device_root` holds the first
                        // listing behind the identification chain only when
                        // it can see the query coming, and deferring after
                        // would let the listing win the port (a rescan
                        // whose probe no longer runs --- `probed_port` is
                        // already set, the script belief is stale --- issues
                        // it straight away).
                        self.defer_device_info_query();
                        self.load_device_root();
                    }
                }
                Some(Err(error)) => {
                    self.devices.set_failed(error.clone());
                    self.set_device_pane_error(error);
                    self.device_disconnected();
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
            if let Some(running) = update.script_running {
                self.set_script_state(if running {
                    crate::device::ScriptState::Running
                } else {
                    crate::device::ScriptState::Stopped
                });
            }
            if let Some(transfer) = update.transfer {
                self.apply_transfer(transfer);
            }
            if let Some(plan) = update.sync_plan {
                self.overlay = Some(Overlay::SyncPreview {
                    plan,
                    confirm: false,
                });
            }
        }

        if let Some(mut build) = self.build.take() {
            let caps = self.manager.capabilities();
            let notices = build.on_process(event, &caps);
            // The flash contents may have just changed under a build-panel
            // command; read the flag before the panel goes back.
            let flashed = build.take_flash_finished();
            self.build = Some(build);
            for (level, message) in notices {
                self.logs.push(level, message);
            }
            if flashed {
                self.reidentify_firmware_after_build_flash();
            }
        }

        if let Some(mut flash) = self.flash.take() {
            let update = flash.on_process(event);
            let fetch_update = flash.on_curl_process(event);
            let chip_query_finished = update.background_chip_query_finished;
            let firmware_read_finished = update.background_firmware_read_finished;
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

            // An erase or write-flash changed what the flash carries: the
            // verdict that gated the listing is stale, and the next listing
            // (`r`, or a device reselect) re-identifies instead of trusting
            // it.
            if update.firmware_invalidated {
                self.firmware_check_port = None;
                self.firmware_check = flash_view::FirmwareCheck::Idle;
                if let Some(flash) = &mut self.flash {
                    flash.clear_firmware_identity();
                }
                // The REPL-banner version belonged to whichever firmware sat
                // on the flash before; a write/erase makes it as stale as
                // the verdict above, and `probed_port`/the script belief
                // must clear too or the probe that would re-read it never
                // runs again on this same port.
                self.mpy_version = None;
                self.probed_port = None;
                self.set_script_state(crate::device::ScriptState::Unknown);
            }
            if firmware_read_finished {
                self.firmware_check = flash_view::FirmwareCheck::Idle;
                // A foreign firmware (or a blank chip) means there was
                // never a MicroPython script to bring back: the "running
                // script" the probe saw was the firmware printing its
                // boot banner, so the restore question that an accepted
                // interruption armed has nothing to offer.
                let foreign = self
                    .flash
                    .as_ref()
                    .and_then(|flash| flash.details.firmware.clone())
                    .is_some_and(|verdict| {
                        !matches!(
                            verdict,
                            crate::firmware_id::FirmwareVerdict::Firmware(
                                crate::firmware_id::FlashFirmware::MicroPython,
                                _
                            )
                        )
                    });
                if foreign {
                    self.restore_pending = false;
                }
            }
            // The background chip query gates the first device listing on
            // a newly selected device; its success arms the firmware read
            // the listing waits on next, and either finishing re-drives
            // the chain.
            if chip_query_finished {
                self.arm_firmware_check();
            }
            if chip_query_finished || firmware_read_finished {
                self.drive_held_root_listing();
            }
        }

        // The deferred query (above, and from `apply_device_picker`) can only
        // start once every port holder is gone and the user has nothing to
        // answer --- checked here for promptness and again on every tick,
        // because with a held queue (a running script) no process event ever
        // arrives to reach this line.
        if self.maybe_run_deferred_flash_query() == flash_view::DeferredQuery::Dropped {
            self.drive_held_root_listing();
        }
        self.maybe_run_deferred_firmware_check();
        self.maybe_run_deferred_version_hunt();
        self.drive_held_root_listing();

        // A completed command may have armed either follow-up question.
        self.check_interrupt_gate();
        self.maybe_offer_restore();
    }

    fn set_device_pane_error(&mut self, message: impl Into<String>) {
        if let Some(browser) = &mut self.browser {
            browser.set_device_error(message);
        }
    }

    /// Whether an interactive device REPL/monitor session is currently
    /// eating every keystroke --- shared by [`App::on_key`] (to route bytes
    /// into the pty instead of dashboard navigation) and [`App::shortcuts`]
    /// (so the footer stops advertising bindings that cannot fire while this
    /// is true).
    fn is_monitor_active(&self) -> bool {
        self.focus == Focus::Logs
            && self.log_tab == LogTab::Monitor
            && self.monitor_source == MonitorSource::Device
            && self.device_monitor_process.is_some()
    }

    /// Whether the Monitor tab is showing the run output and the run process
    /// is still alive --- Ctrl+C is intercepted here to send a
    /// KeyboardInterrupt (0x03) to the device instead of quitting.
    fn is_run_active(&self) -> bool {
        self.focus == Focus::Logs
            && self.log_tab == LogTab::Monitor
            && self.monitor_source == MonitorSource::Run
            && self.run_process.is_some()
    }

    /// Whether the Monitor tab is currently showing run output (regardless of
    /// whether the process is still running).
    fn is_run_view(&self) -> bool {
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
    fn monitor_max_offset(&self) -> usize {
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

    fn on_key(&mut self, key: KeyEvent) {
        if self.is_monitor_active() {
            if let Some((id, bytes)) = self.device_monitor_process.zip(key_to_bytes(key)) {
                self.processes.write_stdin(id, &bytes);
            }
            return;
        }

        // Ctrl+C during an active run sends a KeyboardInterrupt (0x03) to the
        // device script instead of quitting the TUI.
        if self.is_run_active()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c'))
        {
            if let Some(id) = self.run_process {
                self.processes.write_stdin(id, &[0x03]);
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

    /// Whether row 2's right half shows the build panel: the backend can
    /// build, and there is no device filesystem pane to show there instead.
    /// A backend with both would need a third stop --- none exists today
    /// (MicroPython has no build, Zephyr no filesystem), and the capability
    /// pair is the gate, never the backend kind (`AGENTS.md` §3).
    pub fn build_pane_visible(&self) -> bool {
        self.build.is_some() && self.build_pane_visible_precondition()
    }

    /// Whether row 2's left half shows the workspace pane: the backend
    /// maintains a shared environment ([`Capability::WorkspaceSync`]) and
    /// has no device filesystem --- the same pair of conditions that give it
    /// the build panel. Capability-gated, never backend-kind-gated
    /// (`AGENTS.md` §3).
    pub fn workspace_pane_visible(&self) -> bool {
        let caps = self.manager.capabilities();
        self.workspace.is_some()
            && caps.contains(Capability::WorkspaceSync)
            && !caps.contains(Capability::Filesystem)
    }

    /// The header's `project` field. A backend that makes the project a
    /// question ([`Capability::ProjectSelect`]) answers it with the picked
    /// root --- the build panel's for Zephyr (and only once that root is a
    /// buildable application: picked in the panel, or a launch directory
    /// that already is one), the session's MicroPython pick otherwise.
    /// Until then the field stays empty, because the cwd is not a project
    /// just because ChipTUI was started in it. Other backends keep the
    /// detection root's directory name as before.
    pub fn header_project(&self) -> String {
        if self
            .manager
            .capabilities()
            .contains(Capability::ProjectSelect)
        {
            if let Some(panel) = &self.build {
                return if self.project_gate_ok() {
                    panel
                        .root
                        .file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
                } else {
                    String::new()
                };
            }
            if let Some(picked) = &self.mpy_root {
                return picked
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            }
        }
        self.manager.name().unwrap_or("--").to_string()
    }

    /// The project half of the panel's checklist: whether the current root
    /// is a buildable application. Backends without
    /// [`Capability::ProjectSelect`] have no such question --- the root is
    /// theirs by definition.
    pub fn project_gate_ok(&self) -> bool {
        let Some(panel) = &self.build else {
            return true;
        };
        !self
            .manager
            .capabilities()
            .contains(Capability::ProjectSelect)
            || crate::backend::zephyr::projects::is_buildable(&panel.root)
    }

    /// Whether `Enter` runs this project-panel action: the operation
    /// buttons need both checklist answers (asked in the workspace pane)
    /// first --- a disabled button is dimmed, never hidden.
    pub fn build_action_enabled(&self, action: crate::build::BuildAction) -> bool {
        let Some(panel) = &self.build else {
            return true;
        };
        match action {
            crate::build::BuildAction::Stop => true,
            crate::build::BuildAction::Build(_)
            | crate::build::BuildAction::Flash
            | crate::build::BuildAction::Menuconfig => {
                panel.lifecycle_ready(self.project_gate_ok())
            }
            // Workspace-scoped, not project-scoped: a project/board answer
            // is irrelevant to `west update`/`west sdk list`, which need
            // only a resolved installation.
            crate::build::BuildAction::UpdateZephyr | crate::build::BuildAction::SdkList => self
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.resolved.is_some()),
        }
    }

    /// Focus order for `Tab`/`BackTab`. The file columns are stops whenever
    /// row 2 shows the browser --- which is exactly when the backend has no
    /// build panel claiming the row instead (a build backend without a
    /// device filesystem gets the workspace+build pair, `SPEC.md` §11). The
    /// workspace pane is a stop when it exists, the build panel when it is
    /// visible ([`Self::build_pane_visible`]). The Project/Device info row
    /// is never a stop --- it is informational only.
    fn focus_order(&self) -> Vec<Focus> {
        let mut order = Vec::new();
        let browser_row = !self.build_pane_visible_precondition();
        if browser_row && self.browser.is_some() {
            order.push(Focus::FilesLocal);
            if self.manager.capabilities().contains(Capability::Filesystem) {
                order.push(Focus::FilesDevice);
            }
        }
        if self.workspace_pane_visible() {
            order.push(Focus::Workspace);
        }
        if self.build_pane_visible() {
            order.push(Focus::Build);
        }
        order.push(Focus::Logs);
        order
    }

    fn step_focus(&mut self, forward: bool) {
        let order = self.focus_order();
        let len = order.len();
        if len == 0 {
            return;
        }
        let next = match order.iter().position(|f| *f == self.focus) {
            // The Project pane is off the tour: leaving it forward enters
            // the tour at its first stop, backward at its last --- the
            // detour ends at the tour's ends, whichever way it is left.
            None if forward => 0,
            None => len - 1,
            Some(index) => {
                if forward {
                    (index + 1) % len
                } else {
                    (index + len - 1) % len
                }
            }
        };
        if self.focus == Focus::Project {
            self.focus_before_project = None;
        }
        self.focus = order[next];
    }

    /// `ctrl+p`: the Project pane's own way in (and back out). Entering
    /// saves where focus was (the toggle's way back) and lands the cursor
    /// on the first question still open --- the pane exists to answer what
    /// is missing, so that is where the user is put. A pane with no rows
    /// (no backend selected) is not entered at all: there is nothing to
    /// walk.
    pub fn toggle_project_focus(&mut self) {
        if self.focus == Focus::Project {
            let back = self
                .focus_before_project
                .take()
                .unwrap_or_else(|| self.fallback_pane());
            self.focus = back;
            return;
        }
        if self.project_rows().is_empty() {
            return;
        }
        self.focus_before_project = Some(self.focus);
        self.focus = Focus::Project;
        self.project_cursor = self.first_open_project_row();
    }

    /// The first pane that still exists after the focused one disappeared:
    /// local files (when the row shows the browser), then workspace, then
    /// build, ending at `Logs` --- the tour's order, so a clamp never jumps
    /// backwards.
    fn fallback_pane(&self) -> Focus {
        if !self.build_pane_visible_precondition() && self.browser.is_some() {
            Focus::FilesLocal
        } else if self.workspace_pane_visible() {
            Focus::Workspace
        } else if self.build_pane_visible() {
            Focus::Build
        } else {
            Focus::Logs
        }
    }

    /// Pulls focus back onto a pane that still exists when a backend switch
    /// removed the one it was sitting on: the file columns need both the
    /// browser and the row that shows it, the workspace/build panes need
    /// theirs. Each falls back through [`Self::fallback_pane`].
    fn clamp_focus(&mut self) {
        let browser_row = !self.build_pane_visible_precondition();
        let needs_clamp = match self.focus {
            Focus::Project => self.project_rows().is_empty(),
            Focus::FilesDevice => {
                !browser_row || !self.manager.capabilities().contains(Capability::Filesystem)
            }
            Focus::FilesLocal => !browser_row || self.browser.is_none(),
            Focus::Workspace => !self.workspace_pane_visible(),
            Focus::Build => !self.build_pane_visible(),
            Focus::Logs => false,
        };
        if needs_clamp {
            self.focus = self.fallback_pane();
        }
    }

    fn on_dashboard_key(&mut self, key: KeyEvent) {
        match key.code {
            // `q` only. A reflex `Esc` ("close what is open") must not end
            // the session: with no overlay open it is a no-op here --- every
            // overlay handles its own `Esc` before this point, and the Flash
            // view keeps `Esc` as "back one screen".
            KeyCode::Char('q') => {
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
            // The Project pane's way in, visible in the footer beside the
            // tab tour it deliberately stands outside of. Crossterm labels
            // the byte 0x10 this way, like every Ctrl+letter.
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_project_focus();
                return;
            }
            KeyCode::Char('o') => {
                self.open_picker();
                return;
            }
            KeyCode::Char('t') => {
                self.open_theme_picker();
                return;
            }
            KeyCode::Char('x') => {
                self.open_flash();
                return;
            }
            KeyCode::Char('P') => {
                self.request_home_screen();
                return;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.overlay = Some(OVERLAY_HELP);
                return;
            }
            KeyCode::Char('d') => {
                // `d` re-runs whichever discovery this backend has: the
                // mpremote listing under `Filesystem`, else (a monitor
                // backend with no filesystem, e.g. Zephyr) the plain USB
                // serial walk.
                if self.manager.capabilities().contains(Capability::Filesystem) {
                    self.scan_devices();
                } else if self.manager.capabilities().contains(Capability::Monitor) {
                    self.scan_serial_devices();
                }
                return;
            }
            KeyCode::Char('m') if self.manager.capabilities().contains(Capability::Monitor) => {
                self.open_monitor();
                return;
            }
            // Capital so it never collides with plain `r` (re-detect / reload
            // pane) --- a restart interrupts whatever the board is doing, so
            // it goes through the same confirm-first dialog as the
            // post-edit-reupload prompt rather than firing immediately.
            KeyCode::Char('R') if self.manager.capabilities().contains(Capability::Reset) => {
                self.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });
                return;
            }
            _ => {}
        }

        if matches!(self.focus, Focus::FilesLocal | Focus::FilesDevice)
            && !self.build_pane_visible_precondition()
        {
            self.on_files_key(key);
            return;
        }

        if self.focus == Focus::Project {
            self.on_project_key(key);
            return;
        }

        if self.focus == Focus::Workspace {
            self.on_workspace_key(key);
            return;
        }

        if self.focus == Focus::Build {
            self.on_build_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('s') if self.is_run_view() => {
                self.save_run_output();
            }
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
            Focus::Logs if self.log_tab == LogTab::Monitor => self.monitor_view.viewport.max(1),
            Focus::Logs => self.log_viewport.max(1),
            _ => 5,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            // The Monitor tab tails live output only while following; once
            // the user scrolls it holds its top-anchored position (new
            // output grows the document below the view).
            Focus::Logs if self.log_tab == LogTab::Log => {
                // The log pane scrolls; up means "towards older entries".
                if delta < 0 {
                    self.logs.scroll_up(delta.unsigned_abs(), self.log_viewport);
                } else {
                    self.logs.scroll_down(delta as usize);
                }
            }
            Focus::Logs if self.log_tab == LogTab::Monitor => {
                if delta < 0 {
                    self.monitor_scroll_up(delta.unsigned_abs());
                } else {
                    self.monitor_scroll_down(delta as usize);
                }
            }
            // FilesLocal/FilesDevice/Workspace/Build/Project never reach
            // here: on_dashboard_key routes them to their own key handlers
            // first.
            _ => {}
        }
    }

    fn jump_to_start(&mut self) {
        match self.focus {
            Focus::Logs if self.log_tab == LogTab::Log => {
                self.logs.scroll_up(usize::MAX, self.log_viewport);
            }
            Focus::Logs if self.log_tab == LogTab::Monitor => {
                self.monitor_scroll.following = false;
                self.monitor_scroll.offset = 0;
            }
            _ => {}
        }
    }

    fn jump_to_end(&mut self) {
        match self.focus {
            Focus::Logs if self.log_tab == LogTab::Log => self.logs.scroll_to_bottom(),
            Focus::Logs if self.log_tab == LogTab::Monitor => {
                self.monitor_scroll = MonitorScroll::default();
            }
            _ => {}
        }
    }

    /// Keybindings for the current context, rendered in the footer.
    ///
    /// The dashboard and flash rows come from the binding table
    /// (`help::footer`): the same declarations the help window lists, so
    /// the two surfaces cannot drift apart. The modal rows below are
    /// dialog-local grammars with no help counterpart.
    pub fn shortcuts(&self) -> Vec<(&'static str, &'static str)> {
        // Every other binding below is a lie while the REPL owns the
        // keyboard: `on_key` forwards raw bytes into the pty instead of
        // dispatching them, so the footer must switch to the one escape that
        // actually works (`mpremote repl`'s own, via `key_to_bytes`'s
        // generic Ctrl+letter handling).
        if self.is_monitor_active() {
            return vec![("ctrl+]", "exit REPL/monitor"), ("type", "send to device")];
        }
        match self.overlay {
            Some(Overlay::Help { filtering, .. }) => {
                if filtering {
                    vec![
                        ("type", "filter"),
                        ("↑/↓", "select"),
                        ("enter", "activate"),
                        ("esc", "done"),
                    ]
                } else {
                    vec![
                        ("↑/↓", "select"),
                        ("/", "filter"),
                        ("enter", "activate"),
                        ("esc", "close"),
                    ]
                }
            }
            Some(Overlay::FileViewer) => vec![
                ("↑/↓", "scroll"),
                ("pgup/pgdn", "page"),
                ("e", "edit with $EDITOR"),
                ("q/esc", "close"),
            ],
            Some(Overlay::CreateEntry { .. }) => vec![
                ("type", "name"),
                ("enter", "create ('name/' for a directory)"),
                ("esc", "cancel"),
            ],
            Some(Overlay::RenameEntry { .. }) => {
                vec![("type", "new name"), ("enter", "rename"), ("esc", "cancel")]
            }
            Some(Overlay::BoardPicker { .. }) => vec![
                ("type", "filter"),
                ("↑/↓", "select"),
                ("enter", "pick (saved for this project)"),
                ("esc", "cancel"),
            ],
            Some(Overlay::ShieldPicker { .. }) => vec![
                ("type", "filter"),
                ("↑/↓", "select"),
                ("enter", "pick / (none) clears"),
                ("esc", "cancel"),
            ],
            Some(Overlay::DirPicker { .. }) => vec![
                ("↑/↓", "select"),
                ("enter", "open / accept"),
                ("←", "up"),
                ("esc", "cancel"),
            ],
            Some(Overlay::BuildDirPicker { .. }) => vec![
                ("type", "name"),
                ("↑/↓", "select"),
                ("enter", "choose/create"),
                ("esc", "cancel"),
            ],
            Some(Overlay::ProjectPicker { .. }) => vec![
                ("↑/↓", "select"),
                ("enter", "build this one (this session)"),
                ("esc", "cancel"),
            ],
            Some(Overlay::PackageInstall { .. }) => {
                vec![("type", "package"), ("enter", "install"), ("esc", "cancel")]
            }
            Some(
                Overlay::BackendPicker { .. }
                | Overlay::DevicePicker { .. }
                | Overlay::ThemePicker { .. }
                | Overlay::FirmwarePicker { .. }
                | Overlay::ProjectSetup { .. }
                | Overlay::FileActions { .. }
                | Overlay::RestoreDeviceScript { .. },
            ) => {
                vec![("↑/↓", "select"), ("enter", "apply"), ("esc", "cancel")]
            }
            Some(
                Overlay::Confirm { .. }
                | Overlay::ConfirmBuild { .. }
                | Overlay::ConfirmDownloadOverwrite { .. }
                | Overlay::ConfirmDelete { .. }
                | Overlay::ConfirmUpload { .. }
                | Overlay::ConfirmRestartDevice { .. }
                | Overlay::ConfirmEraseForMicroPython { .. }
                | Overlay::ConfirmInterruptDevice { .. }
                | Overlay::ConfirmSwitchProject { .. }
                | Overlay::SyncPreview { .. },
            ) => {
                vec![
                    ("←/→", "choose"),
                    ("enter", "confirm"),
                    ("y/n", "quick reply"),
                    ("esc", "cancel"),
                ]
            }
            None => help::footer(
                self.view,
                &help::Context {
                    focus: self.focus,
                    caps: self.manager.capabilities(),
                    run_active: self.is_run_active(),
                    run_view: self.is_run_view(),
                    log_tab: self.log_tab,
                    flash_screen: self.flash.as_ref().map(|flash| flash.screen),
                },
            ),
        }
    }
}

/// Tick rate used by the binary. Re-exported here so the loop reads in one place.
pub const TICK_RATE: Duration = crate::event::DEFAULT_TICK_RATE;

fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Crossterm relabels the raw control bytes the terminal sends:
            // 0x00 arrives as Ctrl+Space and 0x1c..=0x1f (Ctrl+\, Ctrl+],
            // Ctrl+^, Ctrl+_) as Ctrl+4..=Ctrl+7. Those must be converted
            // back, not XORed --- '5' ^ 0x40 is 'u', and mpremote exits its
            // REPL/monitor on Ctrl+] (0x1d), like Ctrl+x (0x18).
            let byte = match c {
                ' ' => 0x00,
                '4'..='7' => c as u8 - b'4' + 0x1c,
                _ => c.to_ascii_uppercase() as u8 ^ 0x40,
            };
            Some(vec![byte])
        }
        KeyCode::Char(c) => {
            let mut b = [0; 4];
            Some(c.encode_utf8(&mut b).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7F]), // DEL
        KeyCode::Tab => Some(vec![b'\t']),      // 0x09: the REPL's tab-complete
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
    use help::HelpSection;

    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn app() -> App {
        App::new("/nonexistent-project-dir")
    }

    /// A live-enough monitor session: a slow fake process owns the monitor
    /// slot, and the app sits where `is_monitor_active` is true.
    fn app_in_monitor() -> (App, ProcessId) {
        let mut app = app();
        let command = crate::process::Command::new(format!(
            "{}/tests/fixtures/bin/slow",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg("5");
        let id = app.processes.spawn(command, Duration::from_secs(30));
        app.device_monitor_process = Some(id);
        app.focus = Focus::Logs;
        app.log_tab = LogTab::Monitor;
        app.set_monitor_source(MonitorSource::Device);
        (app, id)
    }

    #[test]
    fn tab_reaches_the_monitor_as_a_raw_0x09() {
        // MicroPython's readline reads byte 9 for tab-completion; a Tab that
        // maps to `None` would be swallowed before reaching the device.
        assert_eq!(
            key_to_bytes(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(vec![b'\t'])
        );
    }

    /// Crossterm reports the terminal's raw 0x00 and 0x1c..=0x1f bytes as
    /// Ctrl+Space and Ctrl+4..=Ctrl+7; `key_to_bytes` must restore the
    /// original byte instead of XORing the relabeled char.
    #[test]
    fn relabeled_control_bytes_reach_the_device_as_themselves() {
        let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        // Ctrl+] arrives as Ctrl+5: mpremote's REPL/monitor exit key. The old
        // XOR turned it into 'u' (0x75).
        assert_eq!(key_to_bytes(ctrl('5')), Some(vec![0x1d]));
        assert_eq!(key_to_bytes(ctrl('4')), Some(vec![0x1c])); // Ctrl+\
        assert_eq!(key_to_bytes(ctrl('6')), Some(vec![0x1e])); // Ctrl+^
        assert_eq!(key_to_bytes(ctrl('7')), Some(vec![0x1f])); // Ctrl+_
        assert_eq!(key_to_bytes(ctrl(' ')), Some(vec![0x00])); // Ctrl+Space
        // The letters were always right, and still are.
        assert_eq!(key_to_bytes(ctrl('x')), Some(vec![0x18]));
        assert_eq!(key_to_bytes(ctrl('c')), Some(vec![0x03]));
        assert_eq!(key_to_bytes(ctrl('d')), Some(vec![0x04]));
    }

    #[test]
    fn monitor_cursor_follows_the_echoed_line() {
        let (mut app, id) = app_in_monitor();

        // Typed text: cursor sits after it.
        app.handle(AppEvent::Process(crate::process::ProcessEvent::Output {
            id,
            text: ">>> ab".to_string(),
        }));
        assert_eq!(app.device_monitor_output, vec![">>> ab".to_string()]);
        assert_eq!(app.monitor_cursor(), Some(6));

        // One backspace echo: the line loses a char and the cursor tracks.
        app.handle(AppEvent::Process(crate::process::ProcessEvent::Output {
            id,
            text: "\x08\x1b[K".to_string(),
        }));
        assert_eq!(app.device_monitor_output, vec![">>> a".to_string()]);
        assert_eq!(app.monitor_cursor(), Some(5));

        // Left arrow moves the cursor without changing the text.
        app.handle(AppEvent::Process(crate::process::ProcessEvent::Output {
            id,
            text: "\x1b[D".to_string(),
        }));
        assert_eq!(app.device_monitor_output, vec![">>> a".to_string()]);
        assert_eq!(app.monitor_cursor(), Some(4));

        app.processes.cancel(id);
    }

    #[test]
    fn monitor_cursor_disappears_when_the_session_owns_no_keyboard() {
        let (mut app, id) = app_in_monitor();
        app.handle(AppEvent::Process(crate::process::ProcessEvent::Output {
            id,
            text: ">>> ".to_string(),
        }));

        // Focus moved off the monitor: no cursor, even mid-session.
        app.focus = Focus::FilesLocal;
        assert_eq!(app.monitor_cursor(), None);

        // Session over: no cursor either.
        app.focus = Focus::Logs;
        app.handle(AppEvent::Process(crate::process::ProcessEvent::Finished {
            id,
            outcome: crate::process::Outcome::Success,
            duration: Duration::ZERO,
        }));
        assert!(app.device_monitor_process.is_none());
        assert_eq!(app.monitor_cursor(), None);
    }

    #[test]
    fn ctrl_c_quits_from_any_context() {
        for overlay in [
            None,
            Some(OVERLAY_HELP),
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
    fn esc_closes_the_overlay_and_never_quits() {
        let mut app = app();
        app.overlay = Some(OVERLAY_HELP);
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.overlay, None);
        assert!(!app.should_quit());

        // With no overlay, esc is a no-op: quitting is `q`'s job, so a
        // reflex esc cannot end the session.
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.overlay, None);
        assert!(!app.should_quit());

        app.handle(key(KeyCode::Char('q')));
        assert!(app.should_quit());
    }

    #[test]
    fn help_walks_the_command_select_and_activates_rows() {
        let mut app = app();
        app.overlay = Some(OVERLAY_HELP);

        // The cursor walks the command rows, wrapping at both ends.
        let count = help::bindings(app.view, HelpSection::Commands).len();
        app.handle(key(KeyCode::Up));
        assert!(matches!(
            app.overlay,
            Some(Overlay::Help { selected, .. }) if selected == count - 1
        ));
        app.handle(key(KeyCode::Down));
        assert!(matches!(
            app.overlay,
            Some(Overlay::Help { selected: 0, .. })
        ));

        // Enter activates the row: the `t` row replays its key, which opens
        // the theme picker.
        let theme = help::bindings(app.view, HelpSection::Commands)
            .iter()
            .position(|row| row.key == "t")
            .unwrap();
        app.overlay = Some(Overlay::Help {
            filter: String::new(),
            filtering: false,
            selected: theme,
        });
        app.handle(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Some(Overlay::ThemePicker { .. })));

        // A row with no replay event (the `?` toggle) just closes.
        let toggle = help::bindings(app.view, HelpSection::Commands)
            .iter()
            .position(|row| row.key == "?")
            .unwrap();
        app.overlay = Some(Overlay::Help {
            filter: String::new(),
            filtering: false,
            selected: toggle,
        });
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn help_filters_and_activates_through_the_filter() {
        let mut app = app();
        app.overlay = Some(OVERLAY_HELP);

        // `/` starts typing; every printable char is filter text (`j` and
        // `k` included, so they must not move the cursor while editing).
        app.handle(key(KeyCode::Char('/')));
        for c in "theme".chars() {
            app.handle(key(KeyCode::Char(c)));
        }
        assert!(matches!(
            app.overlay,
            Some(Overlay::Help { ref filter, filtering: true, .. })
                if filter == "theme"
        ));

        // Enter activates the row the filter left --- "theme" narrows the
        // commands to the one row, so the replay opens the theme picker.
        app.handle(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Some(Overlay::ThemePicker { .. })));
    }

    #[test]
    fn help_esc_leaves_the_filter_before_it_closes() {
        let mut app = app();
        app.overlay = Some(OVERLAY_HELP);

        app.handle(key(KeyCode::Char('/')));
        app.handle(key(KeyCode::Char('z')));
        app.handle(key(KeyCode::Esc));
        // Out of editing, filter kept: the window is still up...
        assert!(matches!(
            app.overlay,
            Some(Overlay::Help { ref filter, filtering: false, .. })
                if filter == "z"
        ));
        // ...and the second esc closes it.
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn tab_cycles_focus_in_both_directions() {
        let mut app = app();
        // No filesystem capability: focus defaults to FilesLocal, but it is
        // not in the focus order (Logs only), so Tab lands on Logs.
        assert_eq!(app.focus, Focus::FilesLocal);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Logs);
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
        // The real flow (main.rs / apply_picker) creates the browser the
        // moment the backend is known; the local column is a focus stop
        // only once there is one to land in.
        app.maybe_scan_devices();
        app.focus = Focus::Logs;
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::FilesLocal);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::FilesDevice);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Logs);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::FilesLocal);
    }

    #[test]
    fn tab_stops_on_workspace_and_build_without_a_device_filesystem() {
        // A build backend without a device filesystem claims the whole row:
        // no file browser at all --- the tour is Workspace -> Build -> Logs,
        // and clamping a backend switch away from MicroPython lands on
        // Workspace, not a pane that no longer exists.
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::Zephyr));
        // Empty fixture /dev: the serial scan finds nothing, so no picker
        // overlay can steal the Tab keys this test drives.
        let empty_dev =
            std::env::temp_dir().join(format!("chiptui-tab-dev-{}", std::process::id()));
        std::fs::create_dir_all(&empty_dev).unwrap();
        app.set_serial_dir(&empty_dev);
        app.maybe_scan_devices();
        assert!(app.browser.is_none(), "no file browser for a build backend");
        assert!(app.workspace.is_some() && app.build.is_some());

        app.focus = Focus::Logs;
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Workspace);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Build);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Logs);

        // A backend switch away from MicroPython while its device column is
        // focused, through the picker --- the real path that re-clamps.
        app.manager.set_override(Some(BackendKind::MicroPython));
        app.focus = Focus::FilesDevice;
        app.handle(key(KeyCode::Char('o')));
        app.handle(key(KeyCode::Down)); // to Zephyr
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.manager.selected_kind(), Some(BackendKind::Zephyr));
        assert_eq!(
            app.focus,
            Focus::Workspace,
            "clamping must land on the workspace pane, the row's first stop"
        );
    }

    #[test]
    fn t_opens_the_theme_picker_on_the_active_theme() {
        let mut app = app();
        app.handle(key(KeyCode::Char('t')));
        let expected = ThemeChoice::all()
            .iter()
            .position(|&candidate| candidate == app.theme_choice())
            .unwrap();
        assert_eq!(
            app.overlay,
            Some(Overlay::ThemePicker { selected: expected })
        );
    }

    #[test]
    fn navigating_the_theme_picker_previews_without_committing() {
        let mut app = app();
        let original = app.theme();
        app.handle(key(KeyCode::Char('t')));
        app.handle(key(KeyCode::Down));
        let Some(Overlay::ThemePicker { selected }) = app.overlay else {
            panic!("theme picker should be open");
        };
        let hovered = ThemeChoice::all()[selected].resolve(app.manager.selected_kind());

        assert_eq!(
            app.theme_palette(),
            hovered.palette(),
            "the hovered row previews live"
        );
        assert_eq!(app.theme(), original, "nothing commits until Enter");

        app.handle(key(KeyCode::Esc));
        assert_eq!(app.overlay, None);
        assert_eq!(app.theme(), original, "Esc reverts for free");
        assert_eq!(app.theme_palette(), original.palette());
    }

    #[test]
    fn picking_a_theme_applies_it_immediately_and_persists_to_the_config() {
        let mut app = app();
        let home = std::env::temp_dir().join(format!("chiptui-theme-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        app.set_home_dir(&home);

        app.handle(key(KeyCode::Char('t')));
        app.handle(key(KeyCode::Down));
        let Some(Overlay::ThemePicker { selected }) = app.overlay else {
            panic!("theme picker should be open");
        };
        let expected = ThemeChoice::all()[selected];
        app.handle(key(KeyCode::Enter));

        assert_eq!(app.overlay, None);
        assert_eq!(app.theme_choice(), expected, "applies without a restart");
        assert_eq!(
            crate::settings::theme(&home.join(".config")).as_deref(),
            Some(expected.slug()),
            "and survives one"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn auto_theme_slug_parses_and_round_trips() {
        assert_eq!(ThemeChoice::from_slug("auto"), Some(ThemeChoice::Auto));
        assert_eq!(ThemeChoice::from_slug("Auto"), Some(ThemeChoice::Auto));
        assert_eq!(
            ThemeChoice::from_slug("catppuccin-mocha"),
            Some(ThemeChoice::Named(
                ratatui_themes::ThemeName::CatppuccinMocha
            ))
        );
        assert_eq!(
            ThemeChoice::from_slug("everforest"),
            Some(ThemeChoice::Named(ratatui_themes::ThemeName::Everforest))
        );
        assert_eq!(ThemeChoice::from_slug("not-a-theme"), None);
        assert_eq!(ThemeChoice::Auto.slug(), "auto");
        assert_eq!(ThemeChoice::Auto.display_name(), "Auto");
        assert_eq!(
            ThemeChoice::all().first().copied(),
            Some(ThemeChoice::Auto),
            "Auto leads the picker's rows"
        );
    }

    #[test]
    fn auto_theme_follows_the_active_backend() {
        let home = std::env::temp_dir().join(format!("chiptui-theme-auto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.set_home_dir(&home);

        // Pick the leading Auto row: the picker opens on the active choice,
        // and `Up` from row `s` reaches row 0 after exactly `s` presses.
        app.handle(key(KeyCode::Char('t')));
        let Some(Overlay::ThemePicker { selected }) = app.overlay else {
            panic!("theme picker should be open");
        };
        for _ in 0..selected {
            app.handle(key(KeyCode::Up));
        }
        assert_eq!(
            app.overlay,
            Some(Overlay::ThemePicker { selected: 0 }),
            "navigation should land on the Auto row"
        );
        app.handle(key(KeyCode::Enter));

        assert_eq!(app.theme_choice(), ThemeChoice::Auto);
        assert_eq!(
            crate::settings::theme(&home.join(".config")).as_deref(),
            Some("auto"),
            "Auto persists under its own slug"
        );

        // No backend is active, so Auto stands in with the default; each
        // backend then recolors the session live, without a restart or a
        // re-pick.
        assert_eq!(app.theme(), ratatui_themes::ThemeName::TokyoNight);
        app.manager.set_override(Some(BackendKind::Zephyr));
        assert_eq!(
            app.theme(),
            ratatui_themes::ThemeName::CatppuccinMocha,
            "a Zephyr session renders in Catppuccin Mocha"
        );
        app.manager.set_override(Some(BackendKind::MicroPython));
        assert_eq!(
            app.theme(),
            ratatui_themes::ThemeName::Everforest,
            "a MicroPython session renders in Everforest"
        );
        app.manager.set_override(None);
        assert_eq!(
            app.theme(),
            ratatui_themes::ThemeName::TokyoNight,
            "back to no backend, back to the stand-in"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn file_actions_without_upload_or_filesystem_are_purely_local() {
        use crate::backend::Backend as _;
        // Zephyr's real capability set: the local pane offers exactly
        // open/view/edit/delete, nothing device-bound.
        let caps = crate::backend::zephyr::ZephyrBackend.capabilities();
        assert_eq!(
            FileAction::for_entry(Side::Local, false, true, None, caps),
            vec![FileAction::View, FileAction::Edit, FileAction::Delete]
        );
        assert_eq!(
            FileAction::for_entry(Side::Local, false, false, None, caps),
            vec![FileAction::Delete]
        );
        assert_eq!(
            FileAction::for_entry(Side::Local, true, false, None, caps),
            vec![FileAction::Open, FileAction::Delete]
        );
        // Even a differing verdict cannot offer a diff without a filesystem:
        // there is no second copy to compare against.
        assert!(
            !FileAction::for_entry(Side::Local, false, true, Some(SyncStatus::Differs), caps)
                .contains(&FileAction::Diff)
        );
    }

    #[test]
    fn file_actions_keep_transfers_under_upload_and_filesystem() {
        use crate::backend::Backend as _;
        // MicroPython's real capability set: unchanged behavior.
        let caps = crate::backend::micropython::MicroPythonBackend.capabilities();
        assert_eq!(
            FileAction::for_entry(Side::Local, true, false, None, caps),
            vec![
                FileAction::Open,
                FileAction::SendToDevice,
                FileAction::Delete
            ]
        );
        assert_eq!(
            FileAction::for_entry(Side::Local, false, true, Some(SyncStatus::Differs), caps),
            vec![
                FileAction::SendToDevice,
                FileAction::Run,
                FileAction::View,
                FileAction::Edit,
                FileAction::Diff,
                FileAction::Delete
            ]
        );
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
    fn monitor_scrolling_leaves_and_resumes_the_tail() {
        let mut app = app();
        app.focus = Focus::Logs;
        app.log_tab = LogTab::Monitor;
        // The renderer-published geometry: 40 wrapped rows in a 10-row pane.
        app.monitor_view = MonitorView {
            rows: 40,
            viewport: 10,
            width: 80,
        };

        app.handle(key(KeyCode::Up));
        assert!(!app.monitor_scroll.following);
        assert_eq!(app.monitor_scroll.offset, 29, "one row up from the tail");

        app.handle(key(KeyCode::PageUp));
        assert_eq!(app.monitor_scroll.offset, 19, "a page is the viewport");

        app.handle(key(KeyCode::Home));
        assert_eq!(app.monitor_scroll.offset, 0);

        app.handle(key(KeyCode::PageDown));
        assert_eq!(app.monitor_scroll.offset, 10);

        app.handle(key(KeyCode::End));
        assert!(app.monitor_scroll.following, "End resumes following");
    }

    #[test]
    fn monitor_scroll_clamps_to_the_published_geometry() {
        let mut app = app();
        app.focus = Focus::Logs;
        app.log_tab = LogTab::Monitor;
        app.monitor_view = MonitorView {
            rows: 10,
            viewport: 10,
            width: 80,
        };

        app.handle(key(KeyCode::PageUp));
        assert_eq!(
            app.monitor_scroll.offset, 0,
            "nothing to scroll when the console fits"
        );
        assert!(
            !app.monitor_scroll.following,
            "the user still left the tail; the thumb distinguishes top from bottom"
        );

        app.handle(key(KeyCode::End));
        app.monitor_view.rows = 0; // e.g. the buffer was cleared
        app.handle(key(KeyCode::Up));
        assert_eq!(app.monitor_scroll.offset, 0, "clamped to an empty console");
    }

    #[test]
    fn switching_the_monitor_source_re_pins_the_new_feed() {
        let mut app = app();
        app.monitor_view = MonitorView {
            rows: 40,
            viewport: 10,
            width: 80,
        };
        app.monitor_scroll_up(5);
        assert!(!app.monitor_scroll.following);

        app.set_monitor_source(MonitorSource::Build);
        assert!(
            app.monitor_scroll.following,
            "a new feed must start at its tail"
        );
        assert_eq!(app.monitor_scroll.offset, 0);
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
        assert_eq!(app.focus, Focus::FilesLocal);
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
        app.focus = Focus::FilesLocal;
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

    /// Redirecting the home has to redirect the config too, or an inherited
    /// `$XDG_CONFIG_HOME` reads the developer's real `config.toml` into
    /// fixtures that expect "nothing configured".
    #[test]
    fn redirecting_the_home_redirects_the_user_config() {
        let mut app = App::new(std::env::temp_dir());
        let home = std::env::temp_dir().join("chiptui-home-fixture");
        app.set_home_dir(&home);
        assert_eq!(
            app.user_config_path(),
            home.join(".config/chiptui/config.toml")
        );
    }

    #[test]
    fn west_availability_follows_the_resolved_workspace() {
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::Zephyr));

        // A workspace naming west's real executable: that file, not `PATH`,
        // is what gets checked.
        let dir = std::env::temp_dir().join(format!("chiptui-west-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write_tool = |name: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            // Permissions are the point of the fixture, but only on unix.
            #[cfg(not(unix))]
            let _ = mode;
            path.display().to_string()
        };
        let runnable = write_tool("west", 0o755);
        let panel = |west: String| {
            crate::workspace::WorkspacePanel::new(
                crate::backend::zephyr::workspace::Resolution::Single(
                    crate::backend::zephyr::workspace::Workspace {
                        dir: dir.clone(),
                        origin: crate::backend::zephyr::workspace::WorkspaceOrigin::UserConfig,
                        zephyr_base: dir.clone(),
                        venv: None,
                        west,
                        sdk: None,
                    },
                ),
                "",
            )
        };
        let west_available = |app: &App| {
            app.tool_status()
                .into_iter()
                .find(|(tool, _)| *tool == "west")
                .map(|(_, available)| available)
                .unwrap()
        };

        app.workspace = Some(panel(runnable));
        assert!(
            west_available(&app),
            "the venv's west is runnable wherever the file is, PATH aside"
        );

        app.workspace = Some(panel(dir.join("missing").display().to_string()));
        assert!(
            !west_available(&app),
            "a workspace naming a west that is not there is genuinely broken"
        );

        // Present but not executable is broken too --- it would reach the
        // build as `Permission denied` at spawn, long after the pane
        // called it available.
        #[cfg(unix)]
        {
            app.workspace = Some(panel(write_tool("west.py", 0o644)));
            assert!(!west_available(&app), "a file is not an executable");
        }

        // The bare program name (no venv, no override) keeps the `PATH`
        // check --- resolution found nothing to override it with.
        app.workspace = Some(panel("west".to_string()));
        assert_eq!(west_available(&app), crate::backend::tool_available("west"));
    }

    #[test]
    fn the_editor_runs_from_the_detection_root_for_a_browser_backend() {
        let root = std::env::temp_dir().join(format!("chiptui-edit-cwd-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut app = App::new(&root);
        app.bootstrap();
        app.manager.set_override(Some(BackendKind::MicroPython));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            app.editor_cwd(),
            root,
            "$EDITOR must open with the project's files, not the launch directory's"
        );
    }

    #[test]
    fn the_editor_runs_from_the_workspace_panes_project_root() {
        let mut app = app();
        app.detect();
        app.manager.set_override(Some(BackendKind::Zephyr));

        let dir = std::env::temp_dir().join(format!("chiptui-edit-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let panel = crate::workspace::WorkspacePanel::new(
            crate::backend::zephyr::workspace::Resolution::Single(
                crate::backend::zephyr::workspace::Workspace {
                    dir: dir.clone(),
                    origin: crate::backend::zephyr::workspace::WorkspaceOrigin::UserConfig,
                    zephyr_base: dir.clone(),
                    venv: None,
                    west: "west".to_string(),
                    sdk: None,
                },
            ),
            "",
        );
        app.workspace = Some(panel);
        // No project picked yet: the file list root is empty, so the editor
        // must not be dropped into a nameless directory.
        assert_ne!(app.editor_cwd(), PathBuf::new());

        let project = dir.join("app");
        std::fs::create_dir_all(&project).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        app.workspace.as_mut().unwrap().set_files_root(&project);
        assert_eq!(
            app.editor_cwd(),
            project,
            "a picked project is what $EDITOR's file explorer must show"
        );
    }
}
