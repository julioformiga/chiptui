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

use crate::browser::Browser;
use crate::console::{ConsoleLine, LineConsole};
use crate::device::{DevicePath, DeviceState};
use crate::flash::FlashPanel;
use crate::logs::LogStore;
use crate::process::{ProcessId, ProcessManager};
use crate::project::ProjectManager;

pub mod build_dashboard_view;
pub mod build_view;
pub mod devices;
pub mod events;
pub use crate::event::AppEvent;
pub mod file_browser;
pub use file_browser::{FileAction, FileViewer, ViewerSource, ViewerState};
pub mod flash_view;
pub mod focus;
pub mod help;
mod install_view;
pub mod keys;
pub mod monitor_view;
pub use monitor_view::{MonitorScroll, MonitorSource, MonitorView};
mod mouse;
pub mod overlay;
pub use overlay::Overlay;
pub mod packages;
pub mod probe;
pub mod project_view;
pub mod terminal;
pub mod theme;
pub use theme::ThemeChoice;
pub mod version_capture;
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
/// detour): the shortcuts overlay's `e` letter (`ctrl+k`) and the digit
/// `1` enter it, `Tab` leaves it onto the tour's first stop. `FilesLocal`/
/// `FilesDevice` are the dashboard's two file-browser columns; each is its
/// own stop so `Tab` walks all columns in one consistent tour instead of a
/// separate sub-focus inside the files row. `Build` is the build panel a
/// backend without a device filesystem shows (`SPEC.md` §10), and
/// `Workspace` the project-files pane beside it (the backend's shared
/// workspace, not the project). `DeviceInfo` is row 1's right half: like
/// the Project pane it stays off the tour (it answers nothing about the
/// environment), but unlike before it *is* focusable --- digit `2` and
/// `ctrl+←/→` from the Environment pane reach it, because its MAC row has
/// an action (`Enter` copies it, the mouse click's twin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Project,
    DeviceInfo,
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
/// friends), the first two questions and the last two reports.
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
    /// Dependency coverage: the requirements `requirements.txt` declares
    /// against what the device's `/lib` already holds. `Enter` installs the
    /// file through `mip` (one command; mip skips what is already there).
    MpyDependencies,
    /// What the next boot runs: the device's `boot.py`/`main.py` compared
    /// against the project's own copies ([`crate::files::SyncStatus`]).
    MpyBoot,
}

/// Which tab row 3 is showing. `Left`/`Right` switch between them while
/// [`Focus::Logs`] holds focus --- unbound otherwise, so the two-column file
/// browser's own `Left`/`Right` handling never collides with this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogTab {
    #[default]
    Log,
    Monitor,
    /// The user's own shell, running in a PTY. Not capability-gated: a
    /// local shell is a UI affordance, not a backend operation, so every
    /// backend's row 3 offers it.
    Terminal,
}

/// Which tab the device pane (row 2's right half) is showing for a backend
/// that both browses a filesystem and flashes: the device listing, or the
/// **Project actions** tab the flash menu became. Switched with `x` (which
/// opens the actions side) and the arrows while the pane holds focus --- on
/// the files side the arrows keep their directory meaning, so only the
/// actions side ever switches. One pane, two tabs, the row-3 grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DevicePaneTab {
    #[default]
    Files,
    Actions,
}

/// Which half of a board/shield picker's body holds the navigation keys:
/// the west list (the default --- filtering and cursor movement) or the
/// details pane. `Tab` hands the keyboard over so the arrows and
/// pgup/pgdn scroll the docs text instead of walking the list; printable
/// keys and `Enter` belong to the list either way, because filtering is
/// the picker's primary interaction and `Enter` always applies the row
/// under the list cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocsFocus {
    #[default]
    List,
    Details,
}

impl DocsFocus {
    /// The other half --- `Tab`'s whole effect.
    pub const fn toggled(self) -> Self {
        match self {
            Self::List => Self::Details,
            Self::Details => Self::List,
        }
    }
}

/// A freshly opened help overlay: no filter, cursor on the first command.
pub const OVERLAY_HELP: Overlay = Overlay::Help {
    filter: String::new(),
    filtering: false,
    selected: 0,
};

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

/// Resolves the stored `[ui] theme` choice, falling back to Tokyo Night on
/// an absent or unparsable slug --- shared by [`App::new`] and `main.rs`'s
/// home screen, which draws before an `App` exists at all.
pub fn resolve_theme(config_dir: &std::path::Path) -> ThemeChoice {
    crate::settings::theme(config_dir)
        .and_then(|slug| ThemeChoice::from_slug(&slug))
        .unwrap_or(ThemeChoice::Named(ratatui_themes::ThemeName::TokyoNight))
}

/// Resolves the stored `[ui] icons` choice, falling back to plain
/// Unicode on an absent or unrecognized value --- a Private Use Area
/// glyph never appears unless the operator opted in. Read once at
/// startup ([`App::new`]) and moved with `home_dir`, the same two reads
/// as the theme; mid-run, `ctrl+i` ([`App::cycle_icon_set`]) steps and
/// re-persists the choice.
pub fn resolve_icons(config_dir: &std::path::Path) -> crate::icons::IconSet {
    crate::settings::icons(config_dir)
        .and_then(|slug| crate::icons::IconSet::from_slug(&slug))
        .unwrap_or_default()
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
    /// The last drawn frame's full area, published by `ui::draw` each frame
    /// (the same contract [`Self::log_viewport`] has for the log pane) so the
    /// mouse hit-testing can recompute the very layout the user is looking
    /// at. `None` before the first frame or while the terminal is too small
    /// to draw the dashboard --- a gesture then has no geometry to land on.
    pub frame_area: Option<ratatui::layout::Rect>,
    /// Height of the docs pane inside the board/shield pickers, published by
    /// the renderer the same way: page-scrolling the details moves by the
    /// rows that were actually drawn.
    pub docs_viewport: usize,
    /// Where the board/shield pickers' west list is scrolled to. The list
    /// renders from the offset the previous frame settled on (ratatui
    /// adjusts it minimally to keep the selection visible) and publishes the
    /// settled value back here --- so a frame never re-anchors a visible
    /// selection at the pane's bottom edge, and a click maps its row through
    /// the offset that was actually drawn. Reset when a picker opens and
    /// when the filter changes (a different list starts from its top).
    pub docs_list_offset: usize,
    /// The build dashboard window's whole state --- see
    /// [`crate::build_dashboard::DashboardState`] for why the overlay
    /// variant carries none of it.
    pub build_dashboard: crate::build_dashboard::DashboardState,
    /// The dashboard list's settled scroll offset, published by the
    /// renderer and seeded back into the next frame's `ListState` --- the
    /// `docs_list_offset` contract, which is what lets a click on a visible
    /// row select it without re-anchoring the view.
    pub dashboard_list_offset: usize,
    /// The dashboard details pane's drawn height, published so paging and
    /// clamping match what was actually rendered.
    pub dashboard_viewport: usize,
    /// Ticks observed, used for the "detecting" spinner and as a liveness hint.
    pub ticks: u64,
    /// External commands. Owned here so every view shares one drain point.
    pub processes: ProcessManager,
    /// The board/shield pickers' documentation half: the docs.zephyrproject
    /// index, per-entry pictures and detail text that enrich the west lists
    /// ([`crate::board_docs`]). Drained and applied like process events,
    /// driven from the tick's selection watch.
    pub docs: crate::board_docs::BoardDocs,
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
    /// against, plus `west update`.
    pub workspace: Option<crate::workspace::WorkspacePanel>,
    /// The Zephyr installer, created when the user picks a folder to
    /// install into and dropped once the installation is persisted. Holds
    /// its own process slot and output buffer --- the overlay that draws it
    /// carries no state at all.
    pub installer: Option<crate::install::Installer>,
    /// Tool overrides waiting for an installer to exist (the panel is
    /// created long after `bootstrap`). The test seam for `pyenv` and the
    /// prerequisite queries; empty in every real run.
    installer_tool_paths: Vec<(&'static str, String)>,
    /// Rows of installer output the last frame drew, published by the
    /// renderer so page scrolling matches the drawn height --- the same
    /// contract [`Self::log_viewport`] has for the log pane.
    pub install_viewport: usize,
    /// Whether the open `Overlay::Confirm` is the installer's. The shared
    /// confirm is otherwise the flash panel's, which reads its action from
    /// `FlashPanel::pending`.
    pub(crate) install_confirm_pending: bool,
    /// The Project pane's cursor, over its checklist rows (the environment
    /// questions that moved out of the workspace pane into row 1).
    pub project_cursor: usize,
    /// Which half of the merged `Board · Shield` row the keys act on:
    /// `true` the board (the left, required half), `false` the shield.
    /// Switched by `←`/`→` while the row is selected.
    pub board_segment: bool,
    /// Which tab the device pane shows ([`DevicePaneTab`]); only meaningful
    /// while [`Self::device_actions_tab_available`] holds.
    pub device_pane_tab: DevicePaneTab,
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
    /// The micropython-lib index copy behind [`Overlay::Packages`] ---
    /// fetched once per session through `curl`, kept for the session after.
    pub package_index: packages::PackageIndex,
    /// The package manager's keyboard state, kept off the overlay so a
    /// confirmation can replace the window and give it back unchanged.
    pub packages: packages::PackagesState,
    /// `requirements.txt`, polled on the tick instead of read per frame.
    pub requirements: packages::RequirementsCache,
    /// Overrides the `curl` executable used for the index fetch (the test
    /// seam; `None` means "resolve on PATH").
    package_curl_path: Option<String>,
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

    /// The Terminal tab's shell session, spawned in a PTY like the device
    /// monitor (`src/app/terminal.rs`).
    pub terminal_process: Option<ProcessId>,
    /// The shell's terminal: a `vt100` cell grid fed the PTY's raw bytes.
    /// Unlike [`Self::monitor_console`], which edits one line and drops
    /// every attribute, this is a real emulator --- a shell prompt paints
    /// itself in colour, redraws by moving the cursor, and full-screen
    /// programs take over the alternate screen.
    pub terminal: terminal::TerminalSession,
    /// The running shell's name (`bash`, `sh`, ...) for the tab strip's
    /// status line. Empty before the first session.
    pub terminal_program: String,
    /// Set by `ctrl+]`: the shell keeps running and streaming into the tab,
    /// but the keyboard returns to the dashboard. Switching back to the
    /// Terminal tab re-attaches.
    terminal_detached: bool,
    /// The workspace environment the running shell was *born* with
    /// (`terminal_west_env`'s answer at spawn time). A process cannot have
    /// its environment edited from outside, so this is what
    /// `apply_west_env` compares against to know a live session went stale
    /// and must be restarted into the environment that resolves *now*.
    terminal_shell_env: Vec<(String, String)>,
    /// The command the Terminal tab spawns instead of `$SHELL` --- the test
    /// seam that keeps the suite off the developer's real shell (the same
    /// role `Browser::set_tool_path` plays for mpremote).
    terminal_tool: Option<crate::process::Command>,

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
    /// Where the authorization to identify the selected device stands: the
    /// background `esptool chip-id` + firmware read the selection would
    /// like to run **restarts the board**, so they never start without the
    /// user's explicit yes ([`Overlay::ConfirmIdentifyDevice`], default
    /// No). While the question is open the first device listing is held
    /// behind it; a decline releases the listing and skips identification
    /// for that port. See [`flash_view::IdentifyAuth`].
    identify: flash_view::IdentifyAuth,
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
    /// A short-lived, self-closing `west espressif monitor` session listening
    /// for a Zephyr board's boot banner --- the live alternative to guessing
    /// a flash-byte window for a versionless verdict, see [`version_capture`].
    /// `None` whenever no capture is in flight.
    version_capture: Option<version_capture::FirmwareVersionCapture>,
    /// The port the current (or last) live version capture covered; tried
    /// once per selection, same idiom as [`Self::probed_port`] --- a miss or
    /// a timeout falls back to the flash-byte hunt rather than retrying.
    version_capture_port: Option<String>,
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
    /// The button glyphs' rendering --- `Unicode` unless `[ui] icons` in
    /// the user config says `nerd`/`none` ([`resolve_icons`]). No per-frame
    /// derivation, so the draw calls that build a button stack read it
    /// straight off [`Self::icon_set`]; the `ctrl+i` cycle
    /// ([`Self::cycle_icon_set`]) is the one thing that moves it mid-run.
    icons: crate::icons::IconSet,
    /// Memoized [`Self::tool_status`]. The render path asks twice a frame
    /// (once to measure the pane, once to draw it) for an answer that only
    /// changes with the selected backend or the resolved tool locations, and
    /// computing it walks every `PATH` entry per tool. Keyed on those two
    /// inputs rather than invalidated by hand, so a backend override applied
    /// straight to `manager` --- as tests do --- still recomputes.
    tool_status_cache: std::cell::RefCell<Option<workspace_view::ToolStatusMemo>>,
    /// The user asked for the home screen (`P`); the binary's loop reads it
    /// and swaps screens.
    switch_requested: bool,
    should_quit: bool,
    last_port_count: Option<usize>,
    /// Whether this terminal answered the Kitty keyboard protocol probe at
    /// startup (`terminal::TerminalGuard::keyboard_enhanced`, plumbed in by
    /// `main.rs`): decides whether the shortcuts overlay's bare-Ctrl gesture
    /// is live, on top of the `ctrl+k` toggle every terminal gets.
    /// [`Self::set_keyboard_enhanced`] is the test seam.
    keyboard_enhanced: bool,
    /// Whether mouse reporting is on for this session (`terminal::init`'s
    /// flag, from `[ui] mouse` in the user config, mirrored by `main.rs`).
    /// A gesture that arrives while this is off is dropped before any
    /// handler sees it --- a terminal reporting mouse unasked must not move
    /// the cursor. [`Self::set_mouse_enabled`] is the test seam.
    mouse_enabled: bool,
    /// `ctrl+f`'s toggle: row 3 (Log/Monitor/Terminal) claims the whole
    /// dashboard body, panes 1/2 undrawn. Checked at the very top of
    /// [`Self::on_key`], ahead of the monitor/terminal keyboard capture, so
    /// the chord works exactly where it is most wanted --- watching a live
    /// monitor or shell session full width. Turning it on parks focus on
    /// [`Focus::Logs`] (the only pane still visible) and `Tab`/`BackTab`
    /// no-op while it holds, since there is nowhere else to step to.
    pub row3_fullscreen: bool,
    /// Text the UI asked to put on the system clipboard (the MAC row's
    /// click) --- consumed by the binary's loop, like
    /// [`Self::take_pending_command`], because only the loop owns stdout
    /// between frames (`terminal::set_clipboard` writes the escape).
    clipboard_request: Option<String>,
    /// The last list click (pane, row index, when) --- the double-click
    /// detector [`crate::app::mouse`] resets per gesture. Not a click
    /// counter: a click on a different row is a fresh single click.
    last_click: Option<(Focus, usize, std::time::Instant)>,
    /// The shortcuts overlay is showing: every pane dims
    /// (`ui::mod`'s `dashboard_focused`/`dashboard_behind_dialog`) except
    /// the initial letter of each reachable one. Deliberately not an
    /// [`Overlay`] --- it must not block the keyboard the way a modal does,
    /// since letting go of Ctrl (or pressing anything unmapped) simply
    /// closes it rather than requiring its own dismiss key.
    pub shortcuts_overlay_active: bool,
}

impl App {
    pub fn new(start_dir: impl Into<PathBuf>) -> Self {
        let home =
            std::env::var_os("HOME").map_or_else(std::path::PathBuf::new, std::path::PathBuf::from);
        let config_dir = crate::settings::default_config_dir(&home);
        let theme = resolve_theme(&config_dir);
        let icons = resolve_icons(&config_dir);
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
            frame_area: None,
            docs_viewport: 1,
            docs_list_offset: 0,
            build_dashboard: crate::build_dashboard::DashboardState::default(),
            dashboard_list_offset: 0,
            dashboard_viewport: 1,
            ticks: 0,
            processes: ProcessManager::new(),
            docs: crate::board_docs::BoardDocs::new(),
            devices: DeviceState::new(),
            browser: None,
            flash: None,
            build: None,
            workspace: None,
            installer: None,
            installer_tool_paths: Vec::new(),
            install_viewport: 0,
            install_confirm_pending: false,
            project_cursor: 0,
            board_segment: true,
            device_pane_tab: DevicePaneTab::default(),
            mpy_projects: None,
            mpy_projects_invalid: None,
            mpy_root: None,
            mpy_projects_loaded: false,
            package_index: packages::PackageIndex::Idle,
            packages: packages::PackagesState::default(),
            requirements: packages::RequirementsCache::default(),
            package_curl_path: None,
            mpy_version: None,
            pending_command: None,
            viewer: None,
            viewer_viewport: 1,
            pending_edit: None,
            device_monitor_process: None,
            device_monitor_output: Vec::new(),
            monitor_console: LineConsole::new(),
            terminal_process: None,
            terminal: terminal::TerminalSession::new(),
            terminal_program: String::new(),
            terminal_detached: false,
            terminal_shell_env: Vec::new(),
            terminal_tool: None,
            run_process: None,
            run_output: Vec::new(),
            run_console: LineConsole::new(),
            run_script: None,
            run_state: RunState::default(),
            flash_query_pending: false,
            identify: flash_view::IdentifyAuth::Idle,
            held_root_listing: false,
            firmware_check_port: None,
            firmware_check: flash_view::FirmwareCheck::Idle,
            probe: None,
            probed_port: None,
            version_capture: None,
            version_capture_port: None,
            restore_pending: false,
            serial_dir: std::path::PathBuf::from("/dev"),
            config_dir,
            theme,
            icons,
            home_dir: home,
            tool_status_cache: std::cell::RefCell::new(None),
            switch_requested: false,
            should_quit: false,
            last_port_count: None,
            keyboard_enhanced: false,
            mouse_enabled: false,
            row3_fullscreen: false,
            clipboard_request: None,
            last_click: None,
            shortcuts_overlay_active: false,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
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

    /// Test seam for `terminal::TerminalGuard::keyboard_enhanced` ---
    /// `main.rs` calls the real probe once at startup; tests point this at
    /// whichever branch of the hybrid trigger they want to exercise without
    /// a real terminal.
    pub fn set_keyboard_enhanced(&mut self, enhanced: bool) {
        self.keyboard_enhanced = enhanced;
    }

    /// Test seam for the session's mouse-reporting flag, the same standing
    /// `set_keyboard_enhanced` has: `main.rs` sets the real answer once per
    /// project session from the guard; tests enable it to exercise click
    /// handling without a terminal.
    pub fn set_mouse_enabled(&mut self, enabled: bool) {
        self.mouse_enabled = enabled;
    }

    /// Queues `text` for the clipboard and says so in the log --- the
    /// gesture half of the MAC row's copy; the binary's loop performs the
    /// write.
    fn copy_to_clipboard(&mut self, what: &str, text: String) {
        self.logs.success(format!("{what} copied to the clipboard"));
        self.clipboard_request = Some(text);
    }

    /// Consumed by the binary's loop, like [`Self::take_pending_command`].
    pub fn take_clipboard_request(&mut self) -> Option<String> {
        self.clipboard_request.take()
    }

    /// Whether `Enter` runs this project-panel action: the operation
    /// buttons need both checklist answers (asked in the workspace pane)
    /// first --- a disabled button is dimmed, never hidden. While a command
    /// runs, everything but `Stop` is disabled too: the panel's one process
    /// slot is occupied, and the dimmed stack under the live `Stop` box is
    /// what says so.
    pub fn build_action_enabled(&self, action: crate::build::BuildAction) -> bool {
        let Some(panel) = &self.build else {
            return true;
        };
        if action != crate::build::BuildAction::Stop && panel.is_busy() {
            return false;
        }
        match action {
            crate::build::BuildAction::Stop => true,
            crate::build::BuildAction::Build(_)
            | crate::build::BuildAction::Flash
            | crate::build::BuildAction::Menuconfig => {
                panel.lifecycle_ready(self.project_gate_ok())
            }
            // Workspace-scoped, not project-scoped: a project/board answer
            // is irrelevant to `west update`, which needs only a resolved
            // installation.
            crate::build::BuildAction::UpdateZephyr => self
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.resolved.is_some()),
            // The row that exists precisely because nothing is resolved:
            // it is the one environment action with no prerequisite.
            crate::build::BuildAction::InstallZephyr => true,
            // Reached through the Zephyr Actions menu, not a panel row, and
            // deliberately ungated: a build directory that is not
            // configured yet is `west`'s own error to explain in the
            // Monitor (the report's whole point is an existing build).
            crate::build::BuildAction::Dashboard | crate::build::BuildAction::SizeReport => true,
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
    use crate::backend::BackendKind;
    use crate::browser::Side;
    use crate::event::AppEvent;
    use help::HelpSection;
    use std::time::Instant;

    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(row: u16, column: u16) -> AppEvent {
        AppEvent::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn app() -> App {
        App::new("/nonexistent-project-dir")
    }

    #[test]
    fn mouse_gestures_are_dropped_while_reporting_is_off() {
        let mut app = app();
        let before = app.focus;
        // The default: `[ui] mouse` unset (or false) means the session
        // never asked the terminal to report, so a stray gesture --- a
        // terminal reporting unasked --- must not move anything, wherever
        // it lands. The coordinates are meaningless while `on_mouse` is a
        // stub; they stay so the assertion keeps guarding the flag once
        // hit-testing gives them meaning.
        app.handle(click(1, 1));
        assert_eq!(app.focus, before);
        assert!(!app.should_quit);
        // And with reporting on the same gesture is at least harmless.
        app.set_mouse_enabled(true);
        app.handle(click(1, 1));
        assert_eq!(app.focus, before);
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

    /// The monitor's control keys reach the session's pty as their exact
    /// bytes, through the whole running app: idf_monitor/miniterm (the
    /// `west espressif monitor` form) and mpremote both speak control-byte
    /// grammars --- Ctrl+T arms miniterm's menu, Ctrl+] exits it --- so a
    /// key lost or translated anywhere between `on_key` and the child's
    /// stdin breaks every one of them at once. The fixture echoes each
    /// byte it reads without touching the terminal mode itself: the pty's
    /// input discipline is ChipTUI's to set (see `pty_input_mode`), and
    /// this test is the proof it is set --- the same contract the real
    /// monitor's own `console.setup()` relies on, from the first
    /// keystroke instead of from the child's startup.
    #[test]
    fn monitor_control_keys_reach_the_session_as_their_bytes() {
        let mut app = app();
        let command = crate::process::Command::new(format!(
            "{}/tests/fixtures/bin/stdin-hex",
            env!("CARGO_MANIFEST_DIR")
        ));
        let id = app
            .processes
            .spawn_pty(command, Duration::from_secs(30))
            .expect("the session spawned");
        app.device_monitor_process = Some(id);
        app.focus = Focus::Logs;
        app.log_tab = LogTab::Monitor;
        app.set_monitor_source(MonitorSource::Device);

        let ctrl = |c: char| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        // Ctrl+T (menu), Ctrl+R (reset), Ctrl+I (timestamps; byte 0x09,
        // not a Tab event here), a plain letter, then Enter. (Ctrl+] is
        // excluded: it is ChipTUI's stop chord on this tab, never forwarded.)
        app.handle(ctrl('t'));
        app.handle(ctrl('r'));
        app.handle(ctrl('i'));
        app.handle(key(KeyCode::Char('A')));
        app.handle(key(KeyCode::Enter));

        let deadline = Instant::now() + Duration::from_secs(10);
        let output = loop {
            for event in app.processes.drain() {
                app.handle(AppEvent::Process(event));
            }
            let text = app.device_monitor_output.join(" ");
            if text.matches("BYTE").count() >= 5 || Instant::now() > deadline {
                break text;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        app.processes.cancel(id);
        // The input side is raw from spawn (no echo, no canonical
        // buffering, no signals), and the byte mappings stay kernel
        // defaults like miniterm leaves them: ICRNL turns the CR written
        // for Enter into the LF every REPL of this ecosystem reads.
        let expected = ["BYTE 14", "BYTE 12", "BYTE 09", "BYTE 41", "BYTE 0a"];
        for byte in expected {
            assert!(
                output.contains(byte),
                "the session never received {byte}:\n{output}"
            );
        }
        let positions: Vec<_> = expected
            .iter()
            .map(|byte| output.find(byte).expect("asserted above"))
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "the bytes must arrive in the order they were typed:\n{output}"
        );
    }

    /// `ctrl+]` on the Monitor tab is ChipTUI's stop chord: the session is
    /// cancelled from here (both key events crossterm may deliver for the
    /// byte --- `]` and the relabeled `5`), because the child's own exit key
    /// cannot be relied on: the idf_monitor `west espressif monitor` runs
    /// hangs on *any* exit key on kernels without TIOCSTI (>= 6.2; its stop
    /// path unblocks the key read by injecting a byte with TIOCSTI, then
    /// joins the reader --- the removed ioctl leaves the join stuck
    /// forever, reproduced against the vendored 1.1).
    #[test]
    fn ctrl_rbracket_stops_the_monitor_session() {
        let (mut app, id) = app_in_monitor();

        let ctrl = |c: char| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        app.handle(ctrl(']'));

        let deadline = Instant::now() + Duration::from_secs(10);
        while app.device_monitor_process.is_some() && Instant::now() < deadline {
            for event in app.processes.drain() {
                app.handle(AppEvent::Process(event));
            }
        }
        assert!(
            app.device_monitor_process.is_none(),
            "the session must end when ctrl+] stops it"
        );
        assert_eq!(id, id); // the cancelled process id, kept for clarity
    }

    /// The relabeled form of the same byte (crossterm reports the terminal's
    /// raw 0x1d as Ctrl+5) must stop the session exactly like Ctrl+].
    #[test]
    fn the_relabeled_ctrl5_stops_the_monitor_session_too() {
        let (mut app, _id) = app_in_monitor();

        let ctrl = |c: char| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        app.handle(ctrl('5'));

        let deadline = Instant::now() + Duration::from_secs(10);
        while app.device_monitor_process.is_some() && Instant::now() < deadline {
            for event in app.processes.drain() {
                app.handle(AppEvent::Process(event));
            }
        }
        assert!(app.device_monitor_process.is_none());
    }

    /// A live-enough Terminal-tab session: the fake `slow` executable owns
    /// the shell slot (never the developer's real `$SHELL`), and the app
    /// sits where `is_terminal_active` is true.
    fn app_in_terminal() -> (App, ProcessId) {
        let mut app = app();
        app.set_terminal_tool(crate::process::Command::new(format!(
            "{}/tests/fixtures/bin/slow",
            env!("CARGO_MANIFEST_DIR")
        )));
        app.focus = Focus::Logs;
        app.show_terminal_tab();
        let id = app.terminal_process.expect("the shell session started");
        (app, id)
    }

    /// A workspace panel resolved to a synthetic installation with every
    /// environment half present: a venv (whose `bin` must lead `PATH`) and
    /// an SDK location. The paths need not exist --- resolution already
    /// happened, and the panel only carries the answer.
    fn resolved_workspace_panel(dir: &std::path::Path) -> crate::workspace::WorkspacePanel {
        crate::workspace::WorkspacePanel::new(
            crate::backend::zephyr::workspace::Resolution::Single(
                crate::backend::zephyr::workspace::Workspace {
                    dir: dir.to_path_buf(),
                    origin: crate::backend::zephyr::workspace::WorkspaceOrigin::UserConfig,
                    zephyr_base: dir.join("zephyr"),
                    venv: Some(dir.join(".venv")),
                    west: dir.join(".venv/bin/west").display().to_string(),
                    sdk: Some(dir.join("sdk")),
                },
            ),
            "",
        )
    }

    /// The resolved workspace's exported environment reaches the Terminal
    /// tab's shell --- not just the build panel's commands --- so `west`
    /// typed there means what it means in the Actions pane. Proven on a
    /// real child: the seam tool prints `ZEPHYR_BASE`, and the grid shows
    /// the workspace's value.
    #[test]
    fn the_terminal_shell_carries_the_workspace_environment() {
        let dir = std::env::temp_dir().join(format!("chiptui-term-env-{}", std::process::id()));
        let mut app = app();
        app.set_terminal_tool(
            crate::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("printf 'ZB=%s\\n' \"$ZEPHYR_BASE\""),
        );
        app.workspace = Some(resolved_workspace_panel(&dir));
        app.show_terminal_tab();
        let id = app.terminal_process.expect("the shell session started");

        let get = |key: &str| {
            app.terminal_shell_env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            get("ZEPHYR_BASE").as_deref(),
            Some(dir.join("zephyr").display().to_string().as_str()),
            "ZEPHYR_BASE is the workspace's checkout"
        );
        assert_eq!(
            get("VIRTUAL_ENV").as_deref(),
            Some(dir.join(".venv").display().to_string().as_str())
        );
        let path = get("PATH").expect("the venv rewrites PATH");
        assert!(
            path.starts_with(&format!("{}/bin:", dir.join(".venv").display())),
            "the venv's bin leads PATH: {path}"
        );
        assert!(
            get("ZEPHYR_SDK_INSTALL_DIR").is_some(),
            "the SDK rides along"
        );

        // The child saw it too, through the same raw PTY arm as always.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !app.terminal.screen().contents().contains("ZB=/") && Instant::now() < deadline {
            for event in app.processes.drain() {
                app.handle(AppEvent::Process(event));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let screen = app.terminal.screen().contents();
        assert!(
            screen.contains(&format!("ZB={}", dir.join("zephyr").display())),
            "the shell's environment holds the workspace's ZEPHYR_BASE: {screen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        app.processes.cancel(id);
    }

    /// A workspace resolving under a live shell restarts it into the new
    /// environment (a process cannot be edited from outside), and an
    /// unchanged environment restarts nothing: one workspace change, one
    /// restart, not one per refresh.
    #[test]
    fn a_workspace_change_restarts_a_live_shell_into_the_new_environment() {
        let dir = std::env::temp_dir().join(format!("chiptui-term-restart-{}", std::process::id()));
        let (mut app, first) = app_in_terminal();
        assert!(
            app.terminal_shell_env.is_empty(),
            "no workspace resolved, the shell inherits the parent's own"
        );

        app.workspace = Some(resolved_workspace_panel(&dir));
        app.apply_west_env();
        let second = app.terminal_process.expect("a new shell session started");
        assert_ne!(first, second, "the stale session was replaced");
        assert!(
            !app.terminal_shell_env.is_empty(),
            "the replacement was born under the workspace's environment"
        );

        app.apply_west_env();
        assert_eq!(
            app.terminal_process,
            Some(second),
            "the same environment refreshes nothing"
        );

        app.processes.cancel(first);
        app.processes.cancel(second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entering_the_terminal_tab_starts_a_shell() {
        let (mut app, _id) = app_in_terminal();
        assert_eq!(app.log_tab, LogTab::Terminal);
        assert!(app.terminal_process.is_some());
        assert_eq!(app.terminal_program, "slow");
        assert!(app.is_terminal_active());

        // The fake's banner lands in the grid through the raw PTY arm, and
        // the cursor follows it to the column the shell would type at.
        let id = app.terminal_process.unwrap();
        app.handle(AppEvent::Process(crate::process::ProcessEvent::Bytes {
            id,
            data: b"user@board:~$ ".to_vec(),
        }));
        assert!(
            app.terminal
                .screen()
                .contents()
                .starts_with("user@board:~$"),
            "the grid holds the banner: {:?}",
            app.terminal.screen().contents()
        );
        assert_eq!(app.terminal_cursor(), Some((0, 14)));
        app.processes.cancel(id);
    }

    #[test]
    fn the_terminal_tab_is_reachable_without_the_monitor_capability() {
        // No backend at all: the strip still steps onto Terminal, because a
        // local shell is not a backend operation.
        let mut app = app();
        app.set_terminal_tool(crate::process::Command::new(format!(
            "{}/tests/fixtures/bin/slow",
            env!("CARGO_MANIFEST_DIR")
        )));
        app.focus = Focus::Logs;
        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.log_tab, LogTab::Terminal);
        assert!(app.terminal_process.is_some());
        app.processes.cancel(app.terminal_process.unwrap());
    }

    #[test]
    fn stepping_the_strip_lands_on_every_offered_tab() {
        // MicroPython offers all three tabs; the steps walk them and clamp
        // at the ends, one tab per press.
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::MicroPython));
        app.set_terminal_tool(crate::process::Command::new(format!(
            "{}/tests/fixtures/bin/slow",
            env!("CARGO_MANIFEST_DIR")
        )));
        app.focus = Focus::Logs;
        let step = |code: KeyCode| AppEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL));

        app.handle(step(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Monitor);
        app.handle(step(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Terminal);
        // Landing on the tab handed the keyboard to the shell, so the
        // remaining steps detach first --- exactly what a user stepping
        // back off the tab does with ctrl+].
        app.terminal_detached = true;
        // Clamped at the end, never wrapping.
        app.handle(step(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Terminal);
        app.handle(step(KeyCode::Left));
        assert_eq!(app.log_tab, LogTab::Monitor);
        app.handle(step(KeyCode::Left));
        assert_eq!(app.log_tab, LogTab::Log);
        app.handle(step(KeyCode::Left));
        assert_eq!(app.log_tab, LogTab::Log);
        if let Some(id) = app.terminal_process {
            app.processes.cancel(id);
        }
    }

    #[test]
    fn ctrl_c_reaches_the_shell_instead_of_quitting() {
        let (mut app, id) = app_in_terminal();
        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(
            !app.should_quit(),
            "ctrl+c belongs to the shell's foreground job"
        );
        app.processes.cancel(id);
    }

    #[test]
    fn ctrl_right_bracket_detaches_the_shell_without_ending_it() {
        let (mut app, id) = app_in_terminal();

        // Crossterm relabels the raw 0x1d byte as Ctrl+5; both spellings
        // must detach.
        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('5'),
            KeyModifiers::CONTROL,
        )));
        assert!(!app.is_terminal_active(), "the keyboard is back");
        assert!(
            app.terminal_process.is_some(),
            "detaching must not kill the shell"
        );
        assert_eq!(app.terminal_cursor(), None);

        // The dashboard owns the keys again: `q` quits, instead of being
        // typed into the PTY.
        app.handle(key(KeyCode::Char('q')));
        assert!(app.should_quit());
        app.should_quit = false;

        // Returning to the tab re-attaches the still-running shell.
        app.show_terminal_tab();
        assert!(app.is_terminal_active());
        assert_eq!(app.terminal_process, Some(id), "no respawn happened");
        app.processes.cancel(id);
    }

    #[test]
    fn a_finished_shell_frees_the_keyboard_and_keeps_its_transcript() {
        let (mut app, id) = app_in_terminal();
        app.handle(AppEvent::Process(crate::process::ProcessEvent::Finished {
            id,
            outcome: crate::process::Outcome::Success,
            duration: Duration::ZERO,
        }));
        assert!(app.terminal_process.is_none());
        assert!(!app.is_terminal_active());
        assert!(
            app.terminal.screen().contents().contains("[shell ok]"),
            "the transcript stays behind with the exit line: {:?}",
            app.terminal.screen().contents()
        );

        // Entering the tab again starts a fresh shell, transcript and all.
        app.show_terminal_tab();
        let new_id = app.terminal_process.expect("a fresh shell started");
        assert_ne!(new_id, id);
        assert!(
            app.terminal.screen().contents().trim().is_empty(),
            "the new session starts clean: {:?}",
            app.terminal.screen().contents()
        );
        app.processes.cancel(new_id);
    }

    #[test]
    fn a_detached_shell_detaches_across_its_exit() {
        // A shell that exits while detached must not come back attached.
        let (mut app, id) = app_in_terminal();
        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(']'),
            KeyModifiers::CONTROL,
        )));
        app.handle(AppEvent::Process(crate::process::ProcessEvent::Finished {
            id,
            outcome: crate::process::Outcome::Failed { code: Some(1) },
            duration: Duration::ZERO,
        }));
        assert!(app.terminal_process.is_none());
        assert!(!app.terminal_detached);
    }

    #[test]
    fn the_terminal_tab_scrolls_its_transcript_like_the_monitor() {
        let mut app = app();
        app.focus = Focus::Logs;
        // No live shell: the tab shows a finished transcript, whose scroll
        // grammar is the Monitor's.
        app.log_tab = LogTab::Terminal;
        app.monitor_view = MonitorView {
            rows: 10,
            viewport: 4,
            width: 40,
        };
        app.handle(key(KeyCode::Up));
        assert!(!app.monitor_scroll.following, "up leaves the tail");
        assert_eq!(app.monitor_scroll.offset, 5, "one row above the tail");
        app.handle(key(KeyCode::End));
        assert!(app.monitor_scroll.following, "end re-pins to the tail");
    }

    #[test]
    fn the_footer_names_the_terminal_sessions_escapes() {
        let (mut app, id) = app_in_terminal();
        let keys: Vec<&str> = app.shortcuts().iter().map(|(key, _)| *key).collect();
        assert!(keys.contains(&"ctrl+d") && keys.contains(&"ctrl+]"));
        app.processes.cancel(id);
    }

    #[test]
    fn ctrl_c_quits_from_any_context() {
        for overlay in [
            None,
            Some(OVERLAY_HELP),
            Some(Overlay::ThemePicker { selected: 0 }),
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
        // focused: answering the empty-project prompt (`apply_project_setup`)
        // is the real path that re-clamps.
        let home = std::env::temp_dir().join(format!("chiptui-clamp-home-{}", std::process::id()));
        let root = std::env::temp_dir().join(format!("chiptui-clamp-root-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let _ = std::fs::remove_dir_all(&empty_dev);
        let mut switch = App::new(&root);
        switch.set_home_dir(&home);
        switch.set_serial_dir(&empty_dev);
        switch.detect();
        switch.manager.set_override(Some(BackendKind::MicroPython));
        switch.maybe_scan_devices();
        switch.focus = Focus::FilesDevice;
        let zephyr = BackendKind::ALL
            .iter()
            .position(|kind| *kind == BackendKind::Zephyr)
            .unwrap();
        switch.apply_project_setup(zephyr);
        assert_eq!(switch.manager.selected_kind(), Some(BackendKind::Zephyr));
        assert_eq!(
            switch.focus,
            Focus::Workspace,
            "clamping must land on the workspace pane, the row's first stop"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
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
    fn icon_set_resolves_from_the_config_and_falls_back_to_unicode() {
        let home = std::env::temp_dir().join(format!("chiptui-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let config = home.join(".config");
        std::fs::create_dir_all(config.join("chiptui")).unwrap();

        std::fs::write(
            config.join("chiptui/config.toml"),
            "[ui]\nicons = \"nerd\"\n",
        )
        .unwrap();
        assert_eq!(
            resolve_icons(&config),
            crate::icons::IconSet::Nerd,
            "the opt-in is honored"
        );

        std::fs::write(
            config.join("chiptui/config.toml"),
            "[ui]\nicons = \"none\"\n",
        )
        .unwrap();
        assert_eq!(
            resolve_icons(&config),
            crate::icons::IconSet::None,
            "the no-glyphs choice is honored"
        );

        std::fs::write(
            config.join("chiptui/config.toml"),
            "[ui]\nicons = \"not-a-font\"\n",
        )
        .unwrap();
        assert_eq!(
            resolve_icons(&config),
            crate::icons::IconSet::Unicode,
            "an unrecognized value never admits PUA glyphs"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn redirected_home_re_reads_the_icon_set() {
        let home = std::env::temp_dir().join(format!("chiptui-icons-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let config = home.join(".config");
        std::fs::create_dir_all(config.join("chiptui")).unwrap();
        std::fs::write(
            config.join("chiptui/config.toml"),
            "[ui]\nicons = \"nerd\"\n",
        )
        .unwrap();

        let mut app = App::new(std::env::temp_dir());
        app.set_home_dir(&home);
        assert_eq!(
            app.icon_set(),
            crate::icons::IconSet::Nerd,
            "the config the redirected home answers wins"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `ctrl+i` steps the three renderings in declaration order and writes
    /// each answer back, the same apply-and-persist trade the theme picker
    /// makes --- while a *plain* `i` (the device pane's package install)
    /// never touches the set.
    #[test]
    fn ctrl_i_cycles_the_icon_set_and_persists_each_step() {
        let mut app = app();
        let home = std::env::temp_dir().join(format!(
            "chiptui-icons-cycle-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        app.set_home_dir(&home);
        assert_eq!(app.icon_set(), crate::icons::IconSet::Unicode);

        let chord = || AppEvent::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
        app.handle(chord());
        assert_eq!(app.icon_set(), crate::icons::IconSet::Nerd);
        app.handle(chord());
        assert_eq!(app.icon_set(), crate::icons::IconSet::None);
        app.handle(chord());
        assert_eq!(
            app.icon_set(),
            crate::icons::IconSet::Unicode,
            "the cycle wraps"
        );
        // The standing answer is what a restart would reload.
        assert_eq!(
            crate::settings::icons(&home.join(".config")).as_deref(),
            Some("unicode")
        );

        // The unmodified key is not the chord: it keeps falling through to
        // whatever pane holds focus.
        app.handle(key(KeyCode::Char('i')));
        assert_eq!(app.icon_set(), crate::icons::IconSet::Unicode);
        let _ = std::fs::remove_dir_all(&home);
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
        assert!(app.shortcuts().iter().any(|(key, _)| *key == "?"));

        app.overlay = Some(Overlay::ProjectSetup { selected: 0 });
        let keys: Vec<&str> = app.shortcuts().iter().map(|(key, _)| *key).collect();
        assert!(
            !keys.contains(&"r"),
            "pane keys are inert while a modal is open"
        );
    }

    /// The 12 overlays whose `shortcuts()` carries no per-variant hint of
    /// its own: `F1` must still open help from every one of them, and `?`
    /// must join it everywhere except the three that take free text (where
    /// `?` has to land in the field instead).
    #[test]
    fn overlays_with_no_hint_of_their_own_still_reach_help() {
        let text_entry_overlays: Vec<Overlay> = vec![
            Overlay::RenameEntry {
                name: "old".into(),
                input: "old".into(),
            },
            Overlay::BuildDirPicker {
                input: String::new(),
                selected: 0,
            },
        ];
        let other_silent_overlays: Vec<Overlay> = vec![
            Overlay::DirPicker {
                purpose: crate::workspace::DirPurpose::Installation,
                path: std::path::PathBuf::new(),
                selected: 0,
                error: None,
            },
            Overlay::ProjectPicker {
                mpy: false,
                selected: 0,
                error: None,
            },
            Overlay::DevicePicker { selected: 0 },
            Overlay::ThemePicker { selected: 0 },
            Overlay::FirmwarePicker { selected: 0 },
            Overlay::ProjectSetup { selected: 0 },
            Overlay::FileActions {
                side: Side::Local,
                name: "file.py".into(),
                is_dir: false,
                status: None,
                selected: 0,
            },
            Overlay::RestoreDeviceScript {
                selected: 0,
                return_to_packages: false,
            },
            Overlay::ZephyrActions { selected: 0 },
        ];

        for overlay in text_entry_overlays.iter().chain(&other_silent_overlays) {
            let mut app = app();
            app.overlay = Some(overlay.clone());
            app.handle(key(KeyCode::F(1)));
            assert!(
                matches!(app.overlay, Some(Overlay::Help { .. })),
                "F1 must open help from {overlay:?}"
            );
        }

        for overlay in &other_silent_overlays {
            let mut app = app();
            app.overlay = Some(overlay.clone());
            app.handle(key(KeyCode::Char('?')));
            assert!(
                matches!(app.overlay, Some(Overlay::Help { .. })),
                "? must open help from {overlay:?}"
            );
        }

        for overlay in &text_entry_overlays {
            let mut app = app();
            app.overlay = Some(overlay.clone());
            app.handle(key(KeyCode::Char('?')));
            assert!(
                !matches!(app.overlay, Some(Overlay::Help { .. })),
                "? must be typed into the field, not open help, for {overlay:?}"
            );
        }
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
    fn the_chord_switches_the_log_tab_only_while_logs_is_focused() {
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::MicroPython));
        app.focus = Focus::Logs;
        let chord = |code: KeyCode| AppEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL));

        app.handle(chord(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Monitor);
        app.handle(chord(KeyCode::Left));
        assert_eq!(app.log_tab, LogTab::Log);

        // The plain arrows no longer switch any strip: row 3's included.
        app.handle(key(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Log);

        // Elsewhere, Left/Right must not touch it (e.g. reserved for other
        // panes' own navigation).
        app.focus = Focus::FilesLocal;
        app.handle(key(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Log);
    }

    #[test]
    fn ctrl_arrows_switch_the_log_tab_too() {
        // Row 3 keeps its plain arrows (nothing competes with them there)
        // and answers the ctrl chord as well: one key means "switch tabs"
        // wherever a pane has a strip, so the device pane's chord works
        // here by reflex.
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::MicroPython));
        app.focus = Focus::Logs;

        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.log_tab, LogTab::Monitor);
        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.log_tab, LogTab::Log);
    }

    #[test]
    fn the_chord_steps_between_the_panes_of_a_row_without_strips() {
        // Zephyr's working row has no strips anywhere, so the chord's
        // horizontal half answers there: workspace ↔ build, and row 3
        // keeps its place --- one keypress moves exactly one thing.
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::Zephyr));
        // The working row's panes have to exist before they can be stepped
        // between (the dashboard's tick creates them lazily otherwise).
        app.maybe_scan_devices();
        app.focus = Focus::Workspace;

        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.focus, Focus::Build);
        assert_eq!(app.log_tab, LogTab::Log);
        app.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.focus, Focus::Workspace);
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
            keys.contains(&"ctrl+←/→"),
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
    fn a_tick_picks_up_an_external_local_change() {
        let root = std::env::temp_dir().join(format!("chiptui-tick-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.py"), "print('hi')\n").unwrap();
        let mut app = app();
        app.browser = Some(Browser::new(&root));

        // Four ticks is the once-a-second poll cadence.
        for _ in 0..4 {
            app.handle(AppEvent::Tick);
        }
        assert!(
            !app.browser
                .as_ref()
                .unwrap()
                .local_entries
                .iter()
                .any(|entry| entry.name == "added.py")
        );

        std::fs::write(root.join("added.py"), "x = 1\n").unwrap();
        for _ in 0..4 {
            app.handle(AppEvent::Tick);
        }
        assert!(
            app.browser
                .as_ref()
                .unwrap()
                .local_entries
                .iter()
                .any(|entry| entry.name == "added.py"),
            "an external change must reach the Files pane without a keypress"
        );

        let _ = std::fs::remove_dir_all(&root);
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
