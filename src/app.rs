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

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode};
use time::OffsetDateTime;

use crate::backend::zephyr::workspace::Workspace;
use crate::backend::{BackendKind, Capabilities, Capability};
use crate::browser::{Browser, Side};
use crate::console::{ConsoleLine, LineConsole};
use crate::device::{DevicePath, DeviceState};
use crate::event::AppEvent;
use crate::files::SyncStatus;
use crate::flash::FlashPanel;
use crate::install::Installer;
use crate::logs::LogStore;
use crate::process::{ProcessId, ProcessManager};
use crate::project::{DetectionOutcome, DetectionSource, ProjectManager};

pub mod build_view;
pub mod devices;
pub mod file_browser;
pub mod flash_view;
pub mod help;
mod install_view;
mod mouse;
pub mod overlay;
pub mod packages;
pub mod probe;
pub mod project_view;
pub mod terminal;
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
/// detour): the shortcuts overlay's `e` letter (`ctrl+k`) enters it, `Tab`
/// leaves it onto the tour's first stop. `FilesLocal`/
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

/// One reachable target of the shortcuts overlay (`ctrl+k`, or a bare Ctrl
/// press/release where the terminal's Kitty keyboard protocol answers ---
/// [`App::keyboard_enhanced`]): the pane a letter jumps to. A separate
/// enum from [`Focus`] because two letters (`a`/`d`) resolve differently
/// depending on whether the device pane is tabbed, and `Environment` is
/// deliberately off the `Tab` tour (`App::focus_order`) while still being a
/// jump target here. `Home` is the odd one out even among those: it names
/// no pane at all --- it is the header's own "Project" label, and acting on
/// it leaves the dashboard entirely (`App::request_home_screen`) rather
/// than moving focus within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortcutTarget {
    Project,
    Home,
    FilesLocal,
    Workspace,
    DeviceFiles,
    DeviceActions,
    Build,
    Log,
    Monitor,
    Terminal,
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
    /// (`SPEC.md` §7). It fires automatically (detection could not conclude
    /// a backend) and persists the choice to `chiptui.toml`.
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
    /// background the first time it opens, enriched from the Zephyr docs
    /// index (picture + detail text for the row under the cursor ---
    /// [`App::docs`]). The boards themselves live in
    /// [`App::build`] ([`crate::build::BuildPanel::boards`]) like the
    /// viewer's content lives in `App::viewer` --- an overlay holds only
    /// what a keypress changes, so rebuilding it per key never re-clones
    /// the list. `input` is the filter text; `scroll` the details pane's
    /// line offset (the arrows with the details focused, pgup/pgdn always);
    /// `focus` which half `Tab` last handed the keyboard ([`DocsFocus`]).
    BoardPicker {
        input: String,
        selected: usize,
        scroll: u16,
        focus: DocsFocus,
    },
    /// The shield picker: the same filterable list grammar over `west
    /// shields`, with a leading `(none)` row --- the shield is optional, and
    /// that row is how an existing pick gets cleared. The list itself lives
    /// in [`App::build`] ([`crate::build::BuildPanel::shields`]) like the
    /// boards do. `input` is the filter text; `scroll` the details pane's
    /// line offset (the arrows with the details focused, pgup/pgdn always);
    /// `focus` which half `Tab` last handed the keyboard ([`DocsFocus`]).
    ShieldPicker {
        input: String,
        selected: usize,
        scroll: u16,
        focus: DocsFocus,
    },
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
    /// The package manager (`Enter` or `s` on the Dependencies row, the
    /// device pane's `i`, or the Actions tab's own button): one filterable
    /// list over `requirements.txt`, the board's `/lib` and the
    /// micropython-lib index, fetched through `curl` when it first opens.
    ///
    /// Carries nothing: every field lives on [`App::packages`], so the
    /// remove confirmation --- which *replaces* this overlay, the slot
    /// being one deep --- can hand the window back exactly as it was.
    Packages,
    /// "Remove this package?" --- the manager's `Del`. Its own variant
    /// rather than the shared [`Overlay::Confirm`] (already multiplexed
    /// between the flash panel's `pending` and the installer's start
    /// question), because accepting acts on *two* things and the wording
    /// has to name both.
    ConfirmRemovePackage {
        /// The package name, or the whole specification for a git/URL line.
        name: String,
        /// Paths under `/lib` the removal would delete, with whether each
        /// needs a recursive `rm`. Empty when only the file declares it.
        targets: Vec<(crate::device::DevicePath, bool)>,
        /// Whether `requirements.txt` carries a line for it.
        declared: bool,
        confirm: bool,
    },
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
    /// The choice menu behind the `Zephyr Actions` button: update the shared
    /// workspace (`west update`), add SDK toolchains, or generate the build
    /// dashboard (`west build -t dashboard`). Same shape as
    /// [`Self::RestoreDeviceScript`] --- a small list, `j`/`k`/arrows to
    /// move, `Enter` to pick --- except `Esc` here is a plain cancel, not
    /// itself a choice: giving up on "what to run" has no implicit action
    /// the way giving up on "how to restore" does.
    ZephyrActions { selected: usize },
    /// The Zephyr installer: prerequisites, the sequence, and the running
    /// step's output. Carries nothing at all --- every piece of its state
    /// lives on [`App::installer`], which is what lets the panel keep a
    /// process and an output buffer while the overlay value is rebuilt on
    /// each keystroke.
    ZephyrInstall,
    /// The SDK toolchain pick, opened from the installer and returning to
    /// it: the names `west sdk list` reported, multi-selected with space.
    /// An empty pick installs the whole bundle.
    SdkToolchains { selected: usize },
    /// The installation picker refused a directory --- and this is the way
    /// forward from that refusal: install one there, finish a half-built
    /// one, or adopt a complete one sitting in its `zephyr/` subdirectory.
    /// Carries the *picked* folder; the target under it and the wording
    /// are derived at draw time from what is actually there
    /// (`ui::overlay::install_offer`).
    ///
    /// `reason` is the refusal this offer answers. It is shown under the
    /// question *and* is what restores the picker on decline: the overlay
    /// slot is one deep, so an offer that covers the picker has to carry
    /// enough to put it back.
    ///
    /// Its own variant rather than the shared [`Self::Confirm`], whose one
    /// slot is already multiplexed between the flash panel's pending
    /// action and the installer's start question.
    ConfirmInstallHere {
        dir: std::path::PathBuf,
        reason: String,
        confirm: bool,
    },
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
    /// Every label buffer-widths to the same 3 cells before its word: a
    /// genuinely wide emoji gets one space, a narrow glyph like `▶` gets two.
    /// Every emoji here is picked to be `Emoji_Presentation=Yes` on its own
    /// --- `🗑`/`👁` used to be forced wide with a trailing `\u{FE0F}`
    /// (VS16), the same trick `ui::files`'s file-listing icons used for
    /// `⚙️`, and for the same reason it was dropped there: `unicode-width`
    /// scores the VS16 sequence 2, but not every terminal's own font
    /// support agrees, so the two disagreeing about a glyph's width is a
    /// terminal-dependent bug no width math on this side can fix. A
    /// dedicated pictograph codepoint has no such disagreement to have.
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "📂 Open",
            Self::SendToDevice => "📤 Send to device",
            Self::Download => "📥 Download",
            Self::Run => "▶  Run",
            Self::View => "🔍 View",
            Self::Edit => "📝 Edit",
            Self::Diff => "🔀 Diff",
            Self::Delete => "🚮 Delete",
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
    pending_monitor: Option<PendingMonitor>,

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
    tool_status_cache: std::cell::RefCell<Option<ToolStatusMemo>>,
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
            pending_monitor: None,
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
            held_root_listing: false,
            firmware_check_port: None,
            firmware_check: flash_view::FirmwareCheck::Idle,
            probe: None,
            probed_port: None,
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
            clipboard_request: None,
            last_click: None,
            shortcuts_overlay_active: false,
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

    /// `ctrl+i`: steps the icon rendering through its three values in
    /// declaration order (`unicode` → `nerd` → `none` → `unicode`), applies
    /// it immediately (the next frame's button stacks read a different
    /// [`Self::icon_set`]) and persists it the same way the theme picker
    /// does --- a failed write still applies the set for this session, so
    /// it is logged as a warning rather than lost silently. The chord only
    /// ever arrives as `Char('i')` + `CONTROL` on a terminal that answered
    /// the Kitty keyboard protocol probe; a legacy terminal sends Ctrl+I as
    /// plain Tab (byte `0x09`), which keeps its focus-tour meaning there,
    /// and nothing about this arm can fire.
    fn cycle_icon_set(&mut self) {
        let next = match self.icons {
            crate::icons::IconSet::Unicode => crate::icons::IconSet::Nerd,
            crate::icons::IconSet::Nerd => crate::icons::IconSet::None,
            crate::icons::IconSet::None => crate::icons::IconSet::Unicode,
        };
        self.icons = next;
        let name = match next {
            crate::icons::IconSet::Unicode => "unicode",
            crate::icons::IconSet::Nerd => "nerd",
            crate::icons::IconSet::None => "none",
        };
        let config = self.user_config_path();
        match crate::settings::save_icons(&config, name) {
            Ok(()) => self
                .logs
                .info(format!("icon set cycled to {name} ({})", config.display())),
            Err(err) => self.logs.warn(format!(
                "icon set cycled to {name} for this session, but could not save it to {}: {err}",
                config.display()
            )),
        }
    }

    /// The button glyphs' rendering for this session ([`resolve_icons`]).
    /// [`Self::set_icon_set`] is the test seam; `ctrl+i`
    /// ([`Self::cycle_icon_set`]) is the runtime switch.
    pub fn icon_set(&self) -> crate::icons::IconSet {
        self.icons
    }

    /// Points the session at another icon rendering --- the test seam the
    /// render tests use to draw the panes with the Nerd set, the same role
    /// `set_terminal_tool`/`set_keyboard_enhanced` play for theirs. Real
    /// sessions get theirs from `[ui] icons` at startup and switch it with
    /// `ctrl+i` ([`Self::cycle_icon_set`]), which also persists the answer.
    pub fn set_icon_set(&mut self, icons: crate::icons::IconSet) {
        self.icons = icons;
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
        // Seeded here so the very first frame's Dependencies row is right:
        // the tick that keeps it fresh has not fired yet.
        self.reload_requirements();
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
                        self.logs
                            .warn(format!("ambiguous project at {root}: {names}"));
                    }
                    DetectionOutcome::Unknown => {
                        self.logs.warn(format!(
                            "no known project found in {searched} director{} from {root}",
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
            // A paste belongs to whatever owns the keyboard, and only the
            // shell does: nothing else on the dashboard reads text.
            AppEvent::Paste(text) => {
                if self.is_terminal_active() {
                    self.paste_into_terminal(&text);
                }
            }
            AppEvent::Resize { .. } => self.logs.scroll_to_bottom(),
            // A mouse gesture is only ever an alternative trigger for an
            // action the keyboard already owns, and only when the session
            // asked for reporting --- otherwise it is dropped here, before
            // any handler sees it.
            AppEvent::Mouse(event) => {
                if self.mouse_enabled {
                    self.on_mouse(event);
                }
            }
            AppEvent::Tick => {
                self.ticks = self.ticks.wrapping_add(1);
                self.check_device_hotplug();
                self.refresh_local_listings();
                self.refresh_requirements();
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
                // The board/shield pickers' documentation fetches: whatever
                // row the cursor rests on is the one whose picture and
                // details are worth fetching, debounced by the tick.
                self.drive_docs_selection();
            }
            AppEvent::Process(event) => self.on_process(&event),
            AppEvent::Docs(event) => {
                if let Some((level, message)) = self.docs.apply(event) {
                    match level {
                        crate::logs::Level::Error => self.logs.error(message),
                        crate::logs::Level::Warn => self.logs.warn(message),
                        _ => self.logs.info(message),
                    }
                }
            }
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
        // The package index fetch owns its events before any subsystem:
        // curl shares the process pool with mpremote/esptool, and an
        // unrecognized id must still reach whoever it belongs to.
        if self.on_package_index_process(event) {
            return;
        }
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
            // The Terminal tab's shell (PTY): the emulator is fed the raw
            // bytes --- decoding them per read chunk is what turns a
            // powerline glyph straddling the boundary into U+FFFD. Its exit
            // frees the keyboard the way the monitor's does.
            crate::process::ProcessEvent::Bytes { id, data }
                if Some(*id) == self.terminal_process =>
            {
                self.feed_terminal(data);
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome,
                duration: _,
            } if Some(*id) == self.terminal_process => {
                self.terminal_process = None;
                self.terminal_detached = false;
                // Written *through* the emulator, so the epitaph lands in
                // the grid wherever the shell left the cursor --- CR before
                // LF because the grid is a terminal, not a line list.
                self.terminal
                    .write(&format!("\r\n[shell {}]\r\n", outcome.summary()));
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
            if let Some(path) = update.listed {
                self.on_device_listing(&path);
            }
        }

        self.install_on_process(event);

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
                self.reidentify_firmware_after_flash();
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
            // verdict that gated the listing is as stale as the flash it
            // was read from, so it gets the same reload `west flash` gets
            // --- dropped and re-armed, a new identification running on
            // its own once the port frees, rather than waiting for the
            // next listing to re-ask.
            if update.firmware_invalidated {
                self.reidentify_firmware_after_flash();
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

        // The Terminal tab's shell owns the keyboard exactly like the device
        // monitor does: every keystroke becomes bytes in its PTY. One
        // escape, and it is the monitor's own chord --- `ctrl+]`, the byte
        // crossterm relabels Ctrl+5. For mpremote it is the *exit* key; a
        // shell has no use for 0x1d, so the same chord becomes *detach*
        // instead: the shell keeps running (and streaming into the tab)
        // while the keyboard returns to the dashboard.
        if self.is_terminal_active() {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
            {
                self.terminal_detached = true;
                return;
            }
            // `shift+pgup`/`shift+pgdn` reach the scrollback, because plain
            // PageUp now belongs to the shell like every other key. This is
            // the same division a real terminal makes: the emulator keeps
            // the shifted pair for its own history and never forwards it.
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            {
                let page = self.page();
                if key.code == KeyCode::PageUp {
                    self.monitor_scroll_up(page);
                } else {
                    self.monitor_scroll_down(page);
                }
                return;
            }
            let application_cursor = self.terminal.screen().application_cursor();
            if let Some((id, bytes)) = self
                .terminal_process
                .zip(terminal::terminal_key_bytes(key, application_cursor))
            {
                // Typing snaps back to the live screen, the way every
                // terminal does: the output the keystroke provokes is the
                // output the user wants to see.
                self.monitor_scroll = MonitorScroll::default();
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

    /// Whether the device pane can host the **Project actions** tab: the
    /// backend browses a filesystem (the pane exists) *and* can flash or
    /// erase (there are actions to show). The tab strip is drawn whenever
    /// this holds; `x` creates the flash panel and switches to it.
    pub fn device_actions_tab_available(&self) -> bool {
        let caps = self.manager.capabilities();
        self.browser.is_some()
            && caps.contains(Capability::Filesystem)
            && (caps.contains(Capability::Flash) || caps.contains(Capability::EraseFlash))
    }

    /// Whether the device pane is *currently showing* the Project actions
    /// tab --- the state `x` and the pane's arrow keys switch, and the flag
    /// the renderer and the key dispatch branch on.
    pub fn device_actions_tab_active(&self) -> bool {
        self.device_pane_tab == DevicePaneTab::Actions && self.device_actions_tab_available()
    }

    /// The `ctrl+←/→` chord, live from every pane: switch the tabs of the
    /// pane in focus when it has a strip, else the device pane's strip when
    /// one exists (its row is where the user's other hand already is ---
    /// the local files pane's nearest strip), else the Log • Monitor strip.
    /// Two panes never flip at once: one keypress, one strip, chosen by
    /// that priority --- flipping every strip together would make a
    /// single-pane switch impossible to express.
    fn switch_strip_tabs(&mut self, forward: bool) {
        match self.focus {
            // Row 3 is its own strip: the focused pane's strip always wins.
            Focus::Logs => self.switch_log_tab(forward),
            // Everywhere else the device pane's strip takes the chord when
            // one exists --- its row is where the cursor's neighbours are
            // (the local files pane) --- and row 3 answers for panes whose
            // backend has no device strip (the Zephyr row).
            _ if self.device_actions_tab_available() => self.switch_device_pane_tab(),
            _ => self.switch_log_tab(forward),
        }
    }

    fn switch_device_pane_tab(&mut self) {
        match self.device_pane_tab {
            // The way in creates the flash panel the tab draws, exactly as
            // `x` does --- the strip is a second door to the same pane, not
            // a lighter one.
            DevicePaneTab::Files => self.show_device_actions_tab(),
            DevicePaneTab::Actions => self.device_pane_tab = DevicePaneTab::Files,
        }
    }

    /// The tabs row 3 offers: `Log` always, `Monitor` when the backend can
    /// monitor, `Terminal` always (a local shell is not a backend
    /// capability, so nothing gates it).
    fn available_log_tabs(&self) -> Vec<LogTab> {
        let mut tabs = vec![LogTab::Log];
        if self.manager.capabilities().contains(Capability::Monitor) {
            tabs.push(LogTab::Monitor);
        }
        tabs.push(LogTab::Terminal);
        tabs
    }

    /// Steps row 3's strip one tab per press, clamped at the ends --- the
    /// same shape the two-tab strip had (Left on Log stays on Log). A
    /// clamped step is a *no-op*: re-selecting the tab the strip is already
    /// on would re-attach a detached shell, so it must not happen.
    fn switch_log_tab(&mut self, forward: bool) {
        let tabs = self.available_log_tabs();
        let index = tabs
            .iter()
            .position(|tab| *tab == self.log_tab)
            .unwrap_or(0);
        let next = if forward {
            (index + 1).min(tabs.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        if next != index {
            self.select_log_tab(tabs[next]);
        }
    }

    fn select_log_tab(&mut self, tab: LogTab) {
        if tab == LogTab::Terminal {
            // Entering the Terminal tab is the start gesture for its shell
            // (and the re-attach point after a `ctrl+]` detach) --- it never
            // moves focus, so the ctrl chord can flip this strip from
            // another pane without giving up the cursor.
            self.show_terminal_tab();
        } else {
            self.log_tab = tab;
        }
    }

    /// The shortcuts overlay's live targets right now: which letter jumps
    /// where, built fresh from whatever is actually visible --- the same
    /// capability/visibility checks `focus_order`, `workspace_pane_visible`,
    /// `build_pane_visible` and `device_actions_tab_available` already use,
    /// never a fixed table (`AGENTS.md` §1: capabilities, not conditionals).
    /// `Environment` is included even though it is off the `Tab` tour ---
    /// the overlay's whole point is reaching panes a letter away, tour or
    /// not.
    fn shortcut_targets(&self) -> Vec<(char, ShortcutTarget)> {
        let mut targets = Vec::new();
        // Always offered, like `l`/`t` --- the header's "Project" label is
        // drawn regardless of backend or capability, and `shift+p` (the key
        // this mirrors) carries no guard either.
        targets.push(('p', ShortcutTarget::Home));
        if !self.project_rows().is_empty() {
            targets.push(('e', ShortcutTarget::Project));
        }
        let browser_row = !self.build_pane_visible_precondition();
        if browser_row && self.browser.is_some() {
            targets.push(('f', ShortcutTarget::FilesLocal));
            if self.device_actions_tab_available() {
                targets.push(('a', ShortcutTarget::DeviceActions));
                targets.push(('d', ShortcutTarget::DeviceFiles));
            } else if self.manager.capabilities().contains(Capability::Filesystem) {
                targets.push(('d', ShortcutTarget::DeviceFiles));
            }
        }
        if self.workspace_pane_visible() {
            targets.push(('f', ShortcutTarget::Workspace));
        }
        if self.build_pane_visible() {
            targets.push(('a', ShortcutTarget::Build));
        }
        targets.push(('l', ShortcutTarget::Log));
        if self.manager.capabilities().contains(Capability::Monitor) {
            targets.push(('m', ShortcutTarget::Monitor));
        }
        targets.push(('t', ShortcutTarget::Terminal));
        targets
    }

    /// Whether `letter` is currently one of the shortcuts overlay's live
    /// targets --- `ui::mod`'s `shortcut_letter` is the read side, deciding
    /// whether a pane's title highlights it. Only ever `true` while the
    /// overlay itself is up.
    pub(crate) fn is_shortcut_active(&self, letter: char) -> bool {
        self.shortcuts_overlay_active
            && self
                .shortcut_targets()
                .iter()
                .any(|(target_letter, _)| *target_letter == letter)
    }

    /// Jumps to a shortcut target: focuses its pane and, for the two that
    /// share `Focus::FilesDevice`/`Focus::Logs` with a sibling, selects the
    /// right sub-tab too. `Home` is the exception --- it focuses nothing,
    /// it leaves the dashboard the same way `shift+p` does.
    fn apply_shortcut_target(&mut self, target: ShortcutTarget) {
        match target {
            ShortcutTarget::Project => self.focus_project(),
            ShortcutTarget::Home => self.request_home_screen(),
            ShortcutTarget::FilesLocal => self.focus = Focus::FilesLocal,
            ShortcutTarget::Workspace => self.focus = Focus::Workspace,
            ShortcutTarget::DeviceFiles => {
                self.focus = Focus::FilesDevice;
                self.device_pane_tab = DevicePaneTab::Files;
            }
            ShortcutTarget::DeviceActions => {
                self.focus = Focus::FilesDevice;
                self.show_device_actions_tab();
            }
            ShortcutTarget::Build => self.focus = Focus::Build,
            ShortcutTarget::Log => {
                self.focus = Focus::Logs;
                self.select_log_tab(LogTab::Log);
            }
            ShortcutTarget::Monitor => {
                self.focus = Focus::Logs;
                self.select_log_tab(LogTab::Monitor);
            }
            ShortcutTarget::Terminal => {
                self.focus = Focus::Logs;
                self.select_log_tab(LogTab::Terminal);
            }
        }
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

    /// The shortcuts overlay's own key handling, checked before every other
    /// dashboard binding (like `ctrl+←/→`): opening it, closing it,
    /// and resolving a letter to a jump. Returns whether the key was
    /// consumed here --- when it was not, `on_dashboard_key` dispatches
    /// normally.
    ///
    /// Deliberately not routed through `Overlay`/`on_overlay_key`: it must
    /// stay dashboard-wide and must never swallow a key the way a modal
    /// does --- letting go of Ctrl with no letter picked, or pressing
    /// anything unmapped, just closes it.
    fn handle_shortcuts_overlay_key(&mut self, key: KeyEvent) -> bool {
        if self.shortcuts_overlay_active {
            if key.kind == KeyEventKind::Release {
                // The only Release that ever reaches here (`event.rs`'s
                // filter lets through nothing else): Ctrl let go with no
                // letter picked. Cancel.
                self.shortcuts_overlay_active = false;
                return true;
            }
            if let KeyCode::Char(c) = key.code
                && let Some(target) = self
                    .shortcut_targets()
                    .into_iter()
                    .find(|(letter, _)| *letter == c.to_ascii_lowercase())
                    .map(|(_, target)| target)
            {
                self.shortcuts_overlay_active = false;
                self.apply_shortcut_target(target);
                return true;
            }
            // Esc, or any key with no mapped letter: close without acting
            // --- a reflex "get me out of here" beats a stuck overlay.
            self.shortcuts_overlay_active = false;
            return true;
        }

        // The bare-Ctrl gesture: only ever a `Press` here, since a bare
        // Ctrl `Release` with the overlay *not* already open cannot happen
        // (the Press above always opens it first).
        if self.keyboard_enhanced
            && key.kind == KeyEventKind::Press
            && matches!(
                key.code,
                KeyCode::Modifier(ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl)
            )
        {
            self.shortcuts_overlay_active = true;
            return true;
        }

        // The toggle every terminal gets, Kitty-capable or not.
        if key.kind == KeyEventKind::Press
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('k')
        {
            self.shortcuts_overlay_active = true;
            return true;
        }

        false
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
            crate::build::BuildAction::Dashboard => true,
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
        self.focus = order[next];
    }

    /// The Project pane's way in: jumped to by the shortcuts overlay's `e`
    /// letter (`ctrl+k`), with `Tab` re-entering the tour at its first stop
    /// (the pane is a detour, so the tour is the way back out). Entering
    /// lands the cursor on the first question still open --- the pane exists
    /// to answer what is missing, so that is where the user is put. A pane
    /// with no rows (no backend selected) is not entered at all: there is
    /// nothing to walk, and a letter pressed while already inside is a
    /// no-op.
    pub fn focus_project(&mut self) {
        if self.focus == Focus::Project || self.project_rows().is_empty() {
            return;
        }
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
        if self.handle_shortcuts_overlay_key(key) {
            return;
        }

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
            // The strip chord is dashboard-wide: from *any* pane it
            // switches the tabs of the pane in focus when that pane has a
            // strip, the device pane's strip when one exists beside a pane
            // without its own, and the Log • Monitor strip otherwise ---
            // the chord reaches panes beyond the focused one, so a pane's
            // tabs can be switched without giving up the cursor. Placed
            // before the focus dispatch (like `m` and `s`), it also keeps
            // the chord out of the panes' own arrow grammars: on the local
            // files pane it must never descend a directory, in row 3 it
            // merely joins the plain arrows nothing competes with there.
            KeyCode::Left | KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.switch_strip_tabs(key.code == KeyCode::Right);
                return;
            }
            KeyCode::Char('t') => {
                self.open_theme_picker();
                return;
            }
            // The icon cycle: `unicode` → `nerd` → `none`, applied and
            // persisted. The CONTROL guard is what keeps the plain `i` of
            // the device pane's package install falling through to it, and
            // the guard itself only ever passes on a Kitty-protocol
            // terminal --- legacy sends Ctrl+I as Tab, which the arm above
            // keeps as the focus tour.
            KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_icon_set();
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
            // The SDK's toolchains, without walking the whole installer
            // flow to reach them: adding one to an existing SDK is a
            // routine errand (a new board needs a target the bundle was
            // not unpacked with), and it used to cost five keystrokes of
            // navigation through questions that were already answered.
            //
            // Placed here, before the focus dispatch, so it works from
            // whichever pane holds the cursor --- like `m`. The
            // MicroPython `s` (save a captured run's output) is guarded by
            // `is_run_view` further down and belongs to a backend without
            // this capability, so the two never both apply.
            KeyCode::Char('s')
                if self
                    .manager
                    .capabilities()
                    .contains(Capability::WorkspaceSync) =>
            {
                self.open_sdk_toolchains_shortcut();
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

        // The device pane's plain arrows, on the actions side only: the
        // stacked buttons take `↑/↓` alone, so `←/→` are free to walk the
        // strip there (the ctrl chord arrives earlier and works from any
        // pane). On the files side the plain arrows keep their directory
        // meaning (descend / ascend) and fall through to the browser, the
        // same grammar the local pane and an untabbed device pane (no
        // flash capability) always had.
        if self.focus == Focus::FilesDevice
            && self.device_actions_tab_active()
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            self.device_pane_tab = DevicePaneTab::Files;
            return;
        }

        // The device pane's Actions tab: its own grammar (buttons,
        // not a listing), so it takes the keys before the browser does.
        if self.focus == Focus::FilesDevice && self.device_actions_tab_active() {
            self.on_flash_pane_key(key);
            return;
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
            // On the Terminal tab with no shell alive, `r` starts another
            // without leaving the tab --- the recovery from a spawn failure
            // (or from having exited the shell and wanting it back).
            KeyCode::Char('r')
                if self.focus == Focus::Logs
                    && self.log_tab == LogTab::Terminal
                    && self.terminal_process.is_none() =>
            {
                self.start_terminal_shell();
            }
            KeyCode::Char('r') => {
                self.logs.info("re-running project detection");
                self.detect();
                self.maybe_open_project_setup();
            }
            // Plain arrows on the row 3 strip: nothing competes with them
            // there, so they keep the strip to themselves, stepping one tab
            // per press among the tabs this backend offers --- the ctrl
            // chord (which reaches this strip from panes without one of
            // their own) is intercepted earlier, above.
            KeyCode::Left | KeyCode::Right if self.focus == Focus::Logs => {
                self.switch_log_tab(key.code == KeyCode::Right);
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
            Focus::Logs if self.log_tab != LogTab::Log => self.monitor_view.viewport.max(1),
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
            Focus::Logs if self.log_tab != LogTab::Log => {
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
            Focus::Logs if self.log_tab != LogTab::Log => {
                self.monitor_scroll.following = false;
                self.monitor_scroll.offset = 0;
            }
            _ => {}
        }
    }

    fn jump_to_end(&mut self) {
        match self.focus {
            Focus::Logs if self.log_tab == LogTab::Log => self.logs.scroll_to_bottom(),
            Focus::Logs if self.log_tab != LogTab::Log => {
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
            return vec![("ctrl+]", "exit REPL/monitor")];
        }
        // The same truth for the Terminal tab's shell: while it owns the
        // keyboard only two escapes exist --- the shell's own exit (`ctrl+d`
        // or typing `exit`) and the detach chord.
        if self.is_terminal_active() {
            return vec![
                ("ctrl+d", "exit shell"),
                ("ctrl+]", "detach"),
                ("shift+pgup", "scroll back"),
            ];
        }
        // The shortcuts overlay owns the footer while it is up: its own
        // two-key grammar, not whatever the underlying pane would show.
        if self.shortcuts_overlay_active {
            return vec![("letter", "jump to pane")];
        }
        match self.overlay {
            Some(Overlay::Help { filtering, .. }) => {
                // The footer carries only the two keys a reader cannot
                // guess: `/` starts a filter, and `Enter` *activates* the
                // row --- it replays the key after the window closes,
                // which makes the help a launcher, not just a list.
                if filtering {
                    vec![("enter", "activate")]
                } else {
                    vec![("/", "filter"), ("enter", "activate")]
                }
            }
            Some(Overlay::ZephyrInstall) => {
                if self.installer.as_ref().is_some_and(Installer::is_busy) {
                    // The pane's own Stop button carries the way out; the
                    // footer keeps nothing the user cannot guess.
                    vec![]
                } else {
                    vec![("r", "re-check"), ("s", "skip SDK"), ("t", "toolchains")]
                }
            }
            Some(Overlay::ConfirmInstallHere { .. } | Overlay::ConfirmRemovePackage { .. }) => {
                vec![("y/n", "quick reply")]
            }
            Some(Overlay::SdkToolchains { .. }) => vec![("space", "toggle")],
            Some(Overlay::FileViewer) => vec![("e", "edit with $EDITOR")],
            // The trailing-/ convention is the one thing a user cannot
            // guess about the name being typed.
            Some(Overlay::CreateEntry { .. }) => {
                vec![("name/", "for a directory")]
            }
            // These three take free text, so `?` must land in the field ---
            // `F1` is their only way to help (`on_overlay_key`'s
            // `is_text_entry_overlay`).
            Some(Overlay::RenameEntry { .. }) => vec![("F1", "help")],
            // The docs pane on the right answers the scrolling keys ---
            // but only after `Tab` hands it the keyboard, which is the
            // one key a user cannot guess.
            Some(Overlay::BoardPicker { .. }) | Some(Overlay::ShieldPicker { .. }) => vec![
                ("tab", "swap the list/docs focus"),
                ("pgup/pgdn", "scroll the docs pane"),
            ],
            Some(Overlay::DirPicker { .. }) => vec![("?", "help")],
            Some(Overlay::BuildDirPicker { .. }) => vec![("F1", "help")],
            Some(Overlay::ProjectPicker { .. }) => vec![("?", "help")],
            // Free text, so `?` filters rather than opens help --- and no
            // action can live on a plain letter for the same reason. The
            // footer names the gestures the field cannot teach.
            Some(Overlay::Packages) => vec![
                ("enter", "install"),
                ("del", "remove"),
                ("tab", "list/details"),
                ("F1", "help"),
            ],
            Some(
                Overlay::DevicePicker { .. }
                | Overlay::ThemePicker { .. }
                | Overlay::FirmwarePicker { .. }
                | Overlay::ProjectSetup { .. }
                | Overlay::FileActions { .. }
                | Overlay::RestoreDeviceScript { .. }
                | Overlay::ZephyrActions { .. },
            ) => vec![("?", "help")],
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
                // `y`/`n` answer straight from muscle memory --- the only
                // non-obvious key a confirm offers.
                vec![("y/n", "quick reply")]
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
                    actions_tab: self.focus == Focus::FilesDevice
                        && self.device_actions_tab_active(),
                    device_strip: self.device_actions_tab_available(),
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

    /// Every `FileAction` label must buffer-width to 3 cells before its
    /// word --- the padding a wide (2-cell) icon needs is one space less than
    /// a narrow (1-cell) one. This is what a `🗑`/`👁` missing `\u{FE0F}`
    /// breaks: `unicode_width` scores them narrow while most emoji fonts
    /// draw them wide, so the single-space padding used for genuinely wide
    /// icons quietly desyncs the menu from the terminal.
    #[test]
    fn every_file_action_label_budgets_the_same_column() {
        use ratatui::text::Span;

        for action in [
            FileAction::Open,
            FileAction::SendToDevice,
            FileAction::Download,
            FileAction::Run,
            FileAction::View,
            FileAction::Edit,
            FileAction::Diff,
            FileAction::Delete,
        ] {
            let label = action.label();
            let word_start = label
                .char_indices()
                .find(|(_, c)| c.is_alphabetic())
                .map(|(i, _)| i)
                .expect("label has a word");
            let prefix_width = Span::raw(&label[..word_start]).width();
            assert_eq!(
                prefix_width, 3,
                "{label:?} budgets {prefix_width} cells before its word, want 3"
            );
        }
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
        app.handle(key(KeyCode::Right));
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

        app.handle(key(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Monitor);
        app.handle(key(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Terminal);
        // Landing on the tab handed the keyboard to the shell, so the
        // remaining steps detach first --- exactly what a user stepping
        // back off the tab does with ctrl+].
        app.terminal_detached = true;
        // Clamped at the end, never wrapping.
        app.handle(key(KeyCode::Right));
        assert_eq!(app.log_tab, LogTab::Terminal);
        app.handle(key(KeyCode::Left));
        assert_eq!(app.log_tab, LogTab::Monitor);
        app.handle(key(KeyCode::Left));
        assert_eq!(app.log_tab, LogTab::Log);
        app.handle(key(KeyCode::Left));
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
            Overlay::RestoreDeviceScript { selected: 0 },
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
    fn the_chord_falls_to_the_log_strip_on_a_backend_without_a_device_pane() {
        // Zephyr has no device pane to strip, so from its panes the chord
        // lands on row 3 instead --- the chord does something tab-like
        // from every pane, never nothing.
        let mut app = App::new(std::env::temp_dir());
        app.detect();
        app.manager.set_override(Some(BackendKind::Zephyr));
        app.focus = Focus::Workspace;

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
