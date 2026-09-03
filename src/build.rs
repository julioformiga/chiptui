//! The build panel: the backend's build lifecycle as a navigable action
//! list, with the streamed output shown in the Monitor tab
//! (`SPEC.md` §10 --- build, clean, rebuild through the backend's own
//! tools).
//!
//! The panel is a pure state machine like [`crate::browser::Browser`]: it
//! emits notices and appends output, never logging or spawning beyond the
//! [`ProcessManager`] handle it is given. Commands themselves are built by
//! the backend ([`crate::backend::Backend::build_command`]) --- the panel
//! only knows the board and whether a build directory exists, the two facts
//! that shape the list.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use time::{OffsetDateTime, UtcOffset};

use crate::backend::{BuildKind, Capabilities};
use crate::logs::Level;
use crate::process::{Outcome, ProcessEvent, ProcessId, ProcessManager};

/// A Zephyr build from scratch is minutes, not seconds (`FLASH_TIMEOUT`'s
/// 180s would kill a legitimate first build). Half an hour accommodates a
/// cold `west` workspace without letting a wedged compiler live forever.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The build directory every command targets when nothing else is chosen:
/// the ecosystem's own convention.
pub const DEFAULT_BUILD_DIR: &str = "build";

/// `west boards` walks every board root, and `west shields` the shield
/// roots; two minutes covers a full Zephyr SDK checkout without letting a
/// wedged west live forever.
pub const BOARDS_TIMEOUT: Duration = Duration::from_secs(120);

/// Lines of build output kept for the Monitor tab. A full Zephyr build
/// prints thousands; the tail is what diagnoses a failure.
const OUTPUT_CAPACITY: usize = 2_000;

/// The `CMakeCache.txt` entry holding the board a build directory was
/// configured for (`build/zephyr/CMakeCache.txt` on a normal application).
const CACHED_BOARD_KEY: &str = "CACHED_BOARD:STRING=";

/// The `CMakeCache.txt` entry holding the shield that configuration
/// carried, if any. Absent for a build with no shield --- which is not the
/// same as an empty answer, so [`cached_target`] reports it as `None`.
const CACHED_SHIELD_KEY: &str = "SHIELD:STRING=";

/// What a configured build directory says it was built for: the two
/// answers `west build` took at configure time, recovered together.
///
/// Reading them as a pair is what lets a *variant* be recovered whole from
/// a directory that already exists --- a board alone would silently drop
/// the `--shield` half and produce a build that configures differently
/// from the one sitting right there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTarget {
    pub board: String,
    pub shield: Option<String>,
}

/// Result of the last finished command, for the panel's header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildReport {
    /// What ran, as a label ("Build", "Clean", "Flash", …).
    pub what: &'static str,
    pub ok: bool,
    /// Whether the command ran against the host simulator rather than the
    /// board. The state line says so --- `Build (simulator) ok in 3.2s` ---
    /// because nothing else on the pane does: the checklist names the
    /// project's board (which a host build does not change), and the
    /// action stack is the same six rows either way. It follows the report,
    /// so the next build's answer replaces it.
    pub simulator: bool,
    /// The user stopped this command (`Stop`): not a failure, whatever the
    /// exit status says --- the process tree was killed mid-run by design.
    /// `ok` stays `false` (nothing was completed), but the footer and the
    /// Monitor strip draw a stopped report in the warning color with its
    /// own mark instead of the error `✗`, because an outcome the user
    /// chose is not a diagnosis.
    pub cancelled: bool,
    pub duration: Duration,
    /// Wall-clock finish time, in the app's configured local offset.
    pub at: OffsetDateTime,
}

struct Running {
    id: ProcessId,
    what: &'static str,
    /// A successful run leaves a fresh CMakeCache behind (build/rebuild;
    /// `west flash` does not reconfigure). See [`BuildPanel::finish`].
    updates_board: bool,
    /// The action that was started, so [`BuildPanel::finish`] can point the
    /// cursor at where it belongs once the list loses its `Stop` tail ---
    /// `self.cursor` itself is not usable for that: `start` always parks it
    /// on `Stop` the moment a command begins, so by the time it finishes the
    /// row it was launched from is already gone.
    action: BuildAction,
    started: Instant,
    /// The latest step/percentage parsed out of the command's own output
    /// (see [`crate::progress`]) --- `None` until a matching line arrives,
    /// which some commands (`Clean`, `Update Zephyr`) never send.
    progress: Option<crate::progress::Progress>,
}

/// One board *target* from `west boards`: exactly the string
/// `west build -b` takes, its human description, and the vendor.
///
/// A board with cpuclusters produces several targets (`ttgo_t_display_s3`
/// answers `esp32s3/procpu` and `esp32s3/appcpu`), and each becomes its own
/// row: the qualifier is not decoration, it is the only spelling `west
/// build` accepts for such a board. [`parse_boards`] is where one line
/// becomes N rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    /// The full target: `xiao_esp32c3`, `native_sim/native/64`,
    /// `ttgo_t_display_s3/esp32s3/procpu`.
    pub name: String,
    /// The board's `full_name` --- its commercial name.
    pub description: String,
    /// The board's vendor, filtered on but not drawn (the row is already
    /// two columns wide).
    pub vendor: String,
}

/// One shield from `west shields`: the name `west build --shield` takes,
/// its description and its vendor. The same three fields the board rows
/// carry, minus the qualifier expansion --- a shield has no qualifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shield {
    pub name: String,
    pub description: String,
    pub vendor: String,
}

/// Where the panel's board answer came from --- the header's one-word hint,
/// and the guard that keeps a cache read from silently erasing a user pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardOrigin {
    /// Read from `build/zephyr/CMakeCache.txt`.
    Cache,
    /// Chosen in the board picker. Persisted by the app in the project's
    /// registry entry (`SPEC.md` §10, §13 --- never inside the project
    /// directory), which is exactly why it outranks the cache.
    Picked,
    /// Read from the project's registry entry --- the persisted half of a
    /// [`Self::Picked`] answer, re-applied on open. Ranks like a pick, but
    /// belongs to the *project*: a project switch re-derives it instead of
    /// carrying it across.
    Config,
}

/// Where the panel's working directory came from: inherited from the
/// launch directory's detection, or picked in the project picker for this
/// session (never written anywhere --- the picker's answer is a session
/// fact, like a board pick).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectOrigin {
    /// The directory ChipTUI was started in (or its detected project root).
    #[default]
    WorkingDir,
    /// Chosen in the project picker, session-only.
    Picked,
}

impl ProjectOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::WorkingDir => "cwd",
            Self::Picked => "picked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardChoice {
    pub name: String,
    pub origin: BoardOrigin,
}

/// The board list's lifecycle: `west boards` is slow, so it is fetched in
/// the background the first time the picker opens and kept afterwards.
/// Generic because the shield list (`west shields`) walks the same path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListState<T> {
    Idle,
    Loading,
    Loaded(Vec<T>),
    Failed(String),
}

impl<T> Default for ListState<T> {
    /// Every fetch starts idle; the derive macro's `T: Default` bound
    /// would wrongly demand it of the entries (`Idle` carries none).
    fn default() -> Self {
        Self::Idle
    }
}

/// A background list fetch (`west boards`, `west shields`): the state the
/// picker shows, the process id its lines belong to, and the accumulated
/// output the parse runs over at the end --- one lifecycle, shared by every
/// slow `west` listing the pickers need.
#[derive(Debug)]
pub struct ListFetch<T> {
    pub state: ListState<T>,
    process: Option<ProcessId>,
    output: String,
}

impl<T> Default for ListFetch<T> {
    fn default() -> Self {
        Self {
            state: ListState::Idle,
            process: None,
            output: String::new(),
        }
    }
}

impl<T> ListState<T> {
    /// Whether the list is here. The distinction the callers care about is
    /// "can I read entries", not which of the three not-here states it is.
    pub fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded(_))
    }
}

impl<T> ListFetch<T> {
    /// Starts `command` unless a fetch is already running or the list is
    /// already here: each fetch is once per session.
    fn start(&mut self, command: crate::process::Command, processes: &mut ProcessManager) {
        if self.process.is_some() || matches!(self.state, ListState::Loaded(_)) {
            return;
        }
        self.state = ListState::Loading;
        self.output.clear();
        self.process = Some(processes.spawn(command, BOARDS_TIMEOUT));
    }

    /// Drops a finished list so the next [`Self::start`] runs again ---
    /// what a change in the *search roots* calls for: the answer the list
    /// holds was computed for a different set of directories. A fetch still
    /// in flight is left alone; its `Finished` will land on the state this
    /// leaves behind.
    fn invalidate(&mut self) {
        if self.process.is_none() {
            self.state = ListState::Idle;
            self.output.clear();
        }
    }

    /// Records an output line when `id` is this fetch's process. Whether it
    /// was --- each fetch ignores the other's events.
    fn on_line(&mut self, id: ProcessId, text: &str) -> bool {
        if self.process == Some(id) {
            self.output.push_str(text);
            self.output.push('\n');
            true
        } else {
            false
        }
    }

    /// Finishes the fetch when `id` matches, parsing the accumulated output
    /// into the list, or into the failure the picker should explain instead.
    /// `what` names the list and `command` the invocation for the error
    /// messages. Whether the event belonged to this fetch.
    fn on_finished(
        &mut self,
        id: ProcessId,
        outcome: &Outcome,
        parse: impl FnOnce(&str) -> Vec<T>,
        what: &str,
        command: &str,
    ) -> bool {
        if self.process != Some(id) {
            return false;
        }
        self.process = None;
        self.state = match outcome {
            Outcome::Success => ListState::Loaded(parse(&self.output)),
            Outcome::SpawnFailed(reason) => ListState::Failed(format!(
                "could not start the {what} ({reason}) — is west on PATH?"
            )),
            _ => ListState::Failed(format!("{command} failed ({})", outcome.summary())),
        };
        true
    }
}

/// One row of the project panel's action list: operation buttons only ---
/// the `Project path`/`Board` checklist questions live in the workspace
/// pane, next to the other environment answers, so the two panes are "what
/// is defined" and "what runs". The lifecycle buttons stay visible but
/// disabled until both answers exist (see [`BuildPanel::lifecycle_ready`]);
/// `UpdateZephyr` is gated on the resolved workspace instead
/// (`App::build_action_enabled`), since it acts on the shared installation,
/// not this project. `Stop` trails the list exactly
/// while a command runs --- drawn as its own half-width box in the pane's
/// bottom-right corner, not as a row of the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildAction {
    Stop,
    Build(BuildKind),
    /// Writes the built image to the device --- destructive (`SPEC.md` §15),
    /// so it always routes through the confirm overlay.
    Flash,
    /// The interactive Kconfig editor (`west build -t menuconfig`): run with
    /// the terminal suspended like `$EDITOR`, never through the piped
    /// process manager --- its whole value is the interactive screen.
    Menuconfig,
    /// `west update` --- syncs the manifest's projects into the *workspace*,
    /// not this project: slow, network-bound, rewrites checkouts every
    /// project in the workspace shares, so it confirms first like `Clean`/
    /// `Flash` (`SPEC.md` §15). Listed first, ahead of the lifecycle, with
    /// its sibling: the pair acts on the shared installation --- the
    /// environment the workspace pane's checklist resolves --- before any
    /// project action below it has something to run against.
    UpdateZephyr,
    /// `Install Zephyr` --- the row [`Self::UpdateZephyr`] becomes while no
    /// installation is resolved. The two are mutually exclusive by nature
    /// (there is nothing to update before there is something installed, and
    /// nothing to install once there is), so they share one row rather than
    /// each taking their own: the stack stays six buttons, which is the
    /// height `ui::MIN_HEIGHT` was measured against. Opens the directory
    /// picker rather than running anything --- the installation itself is
    /// [`crate::install::Installer`]'s.
    InstallZephyr,
    /// `west build -t dashboard` --- the Zephyr 4.4 build dashboard report,
    /// reached through the Zephyr Actions menu rather than a row of the
    /// stack. Never listed (it only rides a running command, so `finish`
    /// knows where the cursor came from), and never gated: an unconfigured
    /// build directory is `west`'s own error to explain in the Monitor.
    Dashboard,
    /// Runs the host executable a simulator build produced
    /// (`<build>/zephyr/zephyr.exe`).
    ///
    /// **Never a row of the stack**, the way [`Self::Dashboard`] and
    /// [`Self::SizeReport`] are not: the pane's six buttons are the
    /// measured height `ui::MIN_HEIGHT` was fixed against, and this needs
    /// no button --- a build that was answered "simulator" runs what it
    /// built as soon as it succeeds, and running it again is that same
    /// `Build` (incremental, so it costs a link at most). It exists as an
    /// action only so the panel's one process slot can carry it and report
    /// it like anything else.
    Run,
    /// `size_report` --- the memory report the build dashboard's Memory tab
    /// reads, generated on request from that window. [`Self::Dashboard`]'s
    /// twin in every structural way: never a row of the stack, never a
    /// progress shape, and reached from a window rather than from the pane.
    ///
    /// It writes into `<build>/dashboard/`, which is where Zephyr's own
    /// `dashboard` target writes the same three files --- so one run serves
    /// both dashboards and neither pays the minute twice.
    SizeReport,
}

impl BuildAction {
    /// Rows the list shows under `caps`: the workspace-scoped `west update`
    /// first (the shared environment every later action runs in), then
    /// menuconfig (a build-system question answered
    /// before any artifact exists), then the lifecycle in its own order
    /// (clean, build, rebuild), then flash under its capability. With
    /// `running`, `Stop` is appended (cancelling as discoverable as
    /// starting, `SPEC.md` §12); the pane draws it as its own half-width
    /// box in the bottom-right corner rather than a row of the stack.
    /// The lifecycle targets the conventional `build` directory inside the
    /// project --- no directory picker.
    pub fn list(caps: &Capabilities, running: bool) -> Vec<Self> {
        Self::list_for(caps, running, true)
    }

    /// [`Self::list`] with the workspace's state: `installed` picks which
    /// of the two environment rows the first slot carries.
    pub fn list_for(caps: &Capabilities, running: bool, installed: bool) -> Vec<Self> {
        let mut actions = Vec::with_capacity(BuildKind::ALL.len() + 5);
        if caps.contains(crate::backend::Capability::WorkspaceSync) {
            actions.push(if installed {
                Self::UpdateZephyr
            } else {
                Self::InstallZephyr
            });
        }
        actions.push(Self::Menuconfig);
        actions.extend(BuildKind::ALL.iter().map(|kind| Self::Build(*kind)));
        if caps.contains(crate::backend::Capability::Flash) {
            actions.push(Self::Flash);
        }
        if running {
            actions.push(Self::Stop);
        }
        actions
    }
}

pub struct BuildPanel {
    /// Project root: the working directory every command runs in (`west`
    /// finds its workspace from the cwd).
    pub root: PathBuf,
    /// The build directory the lifecycle targets (`-d`): the conventional
    /// `build` until the user picks another, so parallel board
    /// configurations do not keep erasing each other.
    pub build_dir: String,
    /// Whether a Zephyr installation is resolved, pushed in by
    /// [`crate::app::App::refresh_workspace_resolution`]. The panel does not
    /// resolve anything itself --- it only needs the fact to know whether
    /// its first row offers `Update Zephyr` or `Install Zephyr`.
    pub workspace_installed: bool,
    /// The board commands target: from the CMake cache until the user picks
    /// one or a saved answer loads (both outrank, and outlive, cache reads).
    pub board: Option<BoardChoice>,
    /// The optional shield riding on the board answer: picked, or loaded
    /// with the saved board answer, and with no cache fallback --- a shield
    /// is optional, so `None` (build without one) is itself an answer.
    pub shield: Option<String>,
    /// Where `root` came from (the header's hint, and the reason a pick
    /// survives a re-detect).
    pub project_origin: ProjectOrigin,
    pub cursor: usize,
    pub last: Option<BuildReport>,
    pub output: VecDeque<String>,
    /// The project's build variants --- its parallel configurations, each a
    /// board, an optional shield and a build directory of its own. Empty
    /// for a project with a single target, which is the common case and
    /// keeps the panel exactly as it was.
    pub variants: Vec<crate::backend::zephyr::variants::Variant>,
    /// Which of [`Self::variants`] the lifecycle targets. `None` while the
    /// list is empty; otherwise always a valid index, kept so by
    /// [`Self::set_variants`] and [`Self::select_variant`]. A fresh session
    /// starts on the *board* variant, whatever was built last time --- see
    /// [`Self::remembered_simulator`].
    pub variant: Option<usize>,
    /// Which half the build question opens on: `true` the simulator.
    ///
    /// Seeded from the registry when the project opens, so the question
    /// comes back where the last session left it, and updated by every
    /// answer. Deliberately *not* the same thing as [`Self::variant`]: a
    /// session starts targeting the board, so a `Clean` pressed before any
    /// build cannot erase a directory this session never mentioned. The
    /// remembered answer moves a cursor; it does not move the target.
    pub remembered_simulator: bool,
    /// Extra board search roots for the *list* commands: directories a
    /// project-local Zephyr module contributes
    /// ([`crate::backend::zephyr::variants::board_roots`]). Empty for a
    /// project that defines no board of its own.
    ///
    /// They reach `west boards`/`west shields` only. Nothing is injected
    /// into `west build`: what makes an out-of-tree board *buildable* is
    /// the application's own `CMakeLists.txt` pulling the module in
    /// (`ZEPHYR_EXTRA_MODULES`), and inventing a `-DBOARD_ROOT` here would
    /// be the guess `SPEC.md` §8 forbids.
    pub board_roots: Vec<PathBuf>,
    /// The `west boards` fetch, for the board picker.
    pub boards: ListFetch<Board>,
    /// The `west shields` fetch, for the shield picker.
    pub shields: ListFetch<Shield>,
    offset: UtcOffset,
    running: Option<Running>,
    /// Set when a `Flash` command finished successfully and not yet
    /// consumed ([`Self::take_flash_finished`]): the device's flash
    /// contents just changed under a command the esptool flow knows
    /// nothing about, so the caller must re-ask the firmware question.
    flash_finished: bool,
    /// Set when a `SizeReport` command finished successfully and not yet
    /// consumed ([`Self::take_size_report_finished`]): the build dashboard
    /// was closed to run it and wants to come back on the tab that asked.
    /// Only success sets it --- a failure leaves the Monitor holding the
    /// explanation, which a modal over it would hide.
    size_report_finished: bool,
    /// Set when a `Build`/`Rebuild` that targeted the *simulator* finished
    /// successfully and has not been consumed
    /// ([`Self::take_simulator_built`]): building a host target and then
    /// asking the user to press something else to see it is a step with no
    /// decision in it, so the app runs it.
    simulator_built: bool,
    /// Overrides the executable the backend's commands name (`west`), the
    /// seam tests (and later `[tools]` config) plug a substitute into.
    tool_path: Option<String>,
    /// Environment overrides every command carries (the resolved workspace's
    /// `ZEPHYR_BASE` and friends) --- the environment half of the same seam
    /// `tool_path` covers for the executable.
    tool_env: Vec<(String, String)>,
}

impl BuildPanel {
    pub fn new(root: impl Into<PathBuf>, offset: UtcOffset) -> Self {
        let root = root.into();
        let board = cached_board(&root, DEFAULT_BUILD_DIR).map(|name| BoardChoice {
            name,
            origin: BoardOrigin::Cache,
        });
        Self {
            workspace_installed: false,
            root,
            build_dir: DEFAULT_BUILD_DIR.to_string(),
            board,
            shield: None,
            project_origin: ProjectOrigin::default(),
            cursor: 0,
            last: None,
            output: VecDeque::new(),
            variants: Vec::new(),
            variant: None,
            remembered_simulator: false,
            board_roots: Vec::new(),
            boards: ListFetch::default(),
            shields: ListFetch::default(),
            offset,
            running: None,
            flash_finished: false,
            size_report_finished: false,
            simulator_built: false,
            tool_path: None,
            tool_env: Vec::new(),
        }
    }

    /// Sets the extra board search roots (see [`Self::board_roots`]).
    /// A change drops the cached board and shield lists: they were fetched
    /// against the old roots, and the whole point of a root is that it adds
    /// entries the previous answer could not have held.
    pub fn set_board_roots(&mut self, roots: Vec<PathBuf>) {
        if self.board_roots == roots {
            return;
        }
        self.board_roots = roots;
        self.boards.invalidate();
        self.shields.invalidate();
    }

    /// Replaces the variant list, keeping the selection *by name* when the
    /// new list still carries it --- a re-derivation (a fresh board
    /// catalogue arriving, a directory appearing) must not silently switch
    /// which target the buttons act on.
    ///
    /// Selecting is what applies a variant's answers; an empty list clears
    /// the selection and leaves the panel's own board/shield/build-dir
    /// exactly as they were, which is the no-variants behaviour.
    pub fn set_variants(&mut self, variants: Vec<crate::backend::zephyr::variants::Variant>) {
        if self.variants == variants {
            return;
        }
        let current = self.variant_name().map(str::to_string);
        self.variants = variants;
        if self.variants.is_empty() {
            self.variant = None;
            return;
        }
        // Keep the target when the re-derived list still carries it (a
        // fresh board catalogue arriving must not move the build
        // directory under a running session); otherwise land on the board
        // variant, which is where a session starts.
        let index = current
            .and_then(|name| self.variants.iter().position(|v| v.name == name))
            .or_else(|| self.variant_index_for(false))
            .unwrap_or(0);
        self.select_variant(index);
    }

    /// Points the lifecycle at one variant --- which is to say, at its
    /// **build directory**, and nothing else.
    ///
    /// [`Self::board`] and [`Self::shield`] are deliberately left alone.
    /// They are the *project's* answer to "which board is this for",
    /// picked or saved or read from the board's own build cache, and they
    /// are what the environment checklist shows, what the flash confirm
    /// names and what the registry records. A host target is not an answer
    /// to that question --- it is a place a build can run --- so writing
    /// `native_sim/native/64` into them made the checklist, the flash
    /// dialog and the saved `board =` line all claim the simulator was the
    /// project's board.
    ///
    /// The variant's own board and shield reach the build command instead,
    /// through [`Self::build_board`]/[`Self::build_shield`].
    pub fn select_variant(&mut self, index: usize) {
        let Some(variant) = self.variants.get(index).cloned() else {
            return;
        };
        self.variant = Some(index);
        self.build_dir = variant.build_dir;
        self.last = None;
    }

    /// The board the *next build command* passes as `-b`.
    ///
    /// A host variant carries its own, because nothing else names it: the
    /// user never picks `native_sim` and no board cache of the project's
    /// own holds it. Everything else --- including the board variant ---
    /// defers to [`Self::board`], which is where the picked, saved and
    /// cached answers already rank against each other.
    pub fn build_board(&self) -> Option<&str> {
        match self.variant() {
            Some(variant) if variant.is_simulator() => variant.board.as_deref(),
            _ => self.board_name(),
        }
    }

    /// The shield the next build command passes, by the same rule as
    /// [`Self::build_board`]. A host build carries the shield its variant
    /// declares, which is normally none --- there is no board to put one on.
    pub fn build_shield(&self) -> Option<&str> {
        match self.variant() {
            Some(variant) if variant.is_simulator() => variant.shield.as_deref(),
            _ => self.shield_name(),
        }
    }

    /// The selected variant, if the project has any.
    pub fn variant(&self) -> Option<&crate::backend::zephyr::variants::Variant> {
        self.variant.and_then(|index| self.variants.get(index))
    }

    pub fn variant_name(&self) -> Option<&str> {
        self.variant().map(|variant| variant.name.as_str())
    }

    /// Whether the last answered build targets a host build --- no board to
    /// flash, an executable to run instead. False for a project with no
    /// variants: the board answer alone is what such a project has, and
    /// nothing in it says a simulator was ever meant.
    pub fn targets_simulator(&self) -> bool {
        self.variant().is_some_and(|variant| variant.is_simulator())
    }

    /// The project's board variant --- the first that is not a host
    /// target. What `Flash` always writes, whatever the last build was
    /// (there is nothing to flash from a host build), and the left half of
    /// the build question.
    pub fn device_variant(&self) -> Option<&crate::backend::zephyr::variants::Variant> {
        self.variants.iter().find(|v| !v.is_simulator())
    }

    /// The project's host variant, if it keeps one. Its presence is the
    /// whole condition for asking where a build should run.
    pub fn simulator_variant(&self) -> Option<&crate::backend::zephyr::variants::Variant> {
        self.variants.iter().find(|v| v.is_simulator())
    }

    /// Whether the project offers a choice at build time: both a board and
    /// a host target. One of either is no question --- the command starts
    /// outright.
    pub fn offers_build_choice(&self) -> bool {
        self.device_variant().is_some() && self.simulator_variant().is_some()
    }

    /// The index of the variant a build answer selects, `simulator` naming
    /// which half was answered.
    pub fn variant_index_for(&self, simulator: bool) -> Option<usize> {
        self.variants
            .iter()
            .position(|v| v.is_simulator() == simulator)
    }

    /// The build directory `Flash` writes from: the board variant's, never
    /// the host one's.
    ///
    /// Every other command follows the *last build* (`self.build_dir`),
    /// because that is the artifact the user was just looking at. Flash
    /// cannot: a host build produces an executable, not an image, so
    /// following the last build would offer to write a file no runner
    /// understands. With no variants at all this is `build_dir` --- the
    /// project has one target and it is the board's.
    pub fn flash_build_dir(&self) -> String {
        self.device_variant()
            .map(|variant| variant.build_dir.clone())
            .unwrap_or_else(|| self.build_dir.clone())
    }

    pub fn set_tool_path(&mut self, program: impl Into<String>) {
        self.tool_path = Some(program.into());
    }

    pub fn tool_path(&self) -> Option<&str> {
        self.tool_path.as_deref()
    }

    /// Sets the environment overrides every command carries. The app passes
    /// the resolved west workspace's environment here (`ZEPHYR_BASE`, ...);
    /// the panel itself stays backend-agnostic, knowing only "program and
    /// environment overrides".
    pub fn set_tool_env(&mut self, env: Vec<(String, String)>) {
        self.tool_env = env;
    }

    /// Applies the resolved executable and environment to a backend-built
    /// command, next to the cwd the panel also owns.
    ///
    /// The program rewrite is right for every command the backend builds as
    /// `west …`, which is all of them but one --- see [`Self::in_west_env`]
    /// for the exception and why it exists.
    fn decorated(&self, command: crate::process::Command) -> crate::process::Command {
        let command = match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        };
        self.in_west_env(command)
    }

    /// Applies the workspace environment *without* touching the program.
    ///
    /// [`Self::tool_path`] is the resolved `west`, and rewriting a command's
    /// program with it is what makes every lifecycle command run the venv's
    /// west rather than whatever is on `PATH`. Exactly one command this
    /// panel runs is not west: the memory report, whose program is the
    /// venv's *Python* (it runs a script out of the Zephyr checkout, which
    /// has no console-script shim to embed an interpreter). Passing that one
    /// through `decorated` handed the script path to `west`, which read it
    /// as a subcommand and answered `unknown command`.
    fn in_west_env(&self, command: crate::process::Command) -> crate::process::Command {
        command.envs(self.tool_env.clone())
    }

    /// Points the lifecycle at another build directory. The board answer is
    /// re-read from that directory's CMake cache unless the user picked one
    /// for the session (a pick outlives directory switches, same as it
    /// outlives cache refreshes).
    pub fn set_build_dir(&mut self, dir: impl Into<String>) {
        let dir = dir.into();
        if self.build_dir == dir {
            return;
        }
        self.build_dir = dir;
        if self
            .board
            .as_ref()
            .is_none_or(|choice| choice.origin == BoardOrigin::Cache)
        {
            self.board = cached_board(&self.root, &self.build_dir).map(|name| BoardChoice {
                name,
                origin: BoardOrigin::Cache,
            });
        }
        self.cursor = 0;
    }

    /// Points the lifecycle at another project directory (the picker's
    /// answer). Only a hand-picked board survives the re-root: the build
    /// directory, the cached board and a *saved* board belong to the
    /// project being left, so they reset (the caller re-applies the new
    /// project's saved answer, if it has one). The last report is
    /// dropped too: it described the previous project's command.
    pub fn set_project(&mut self, dir: impl Into<PathBuf>) {
        self.root = dir.into();
        self.project_origin = ProjectOrigin::Picked;
        self.build_dir = DEFAULT_BUILD_DIR.to_string();
        if self
            .board
            .as_ref()
            .is_none_or(|choice| choice.origin != BoardOrigin::Picked)
        {
            self.board = cached_board(&self.root, &self.build_dir).map(|name| BoardChoice {
                name,
                origin: BoardOrigin::Cache,
            });
        }
        // The shield rides on the board answer and is project-scoped the
        // same way; with no cache to re-read, it resets to "none".
        self.shield = None;
        // The variants describe the project being left --- its build
        // directories, its `boards/` fragments. The caller re-derives them
        // for the project being entered.
        self.variants.clear();
        self.variant = None;
        self.last = None;
        self.cursor = 0;
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    /// The running command's label ("Build", "Clean", "West update", ...),
    /// for the Monitor tab's live status.
    pub fn running_label(&self) -> Option<&'static str> {
        self.running.as_ref().map(|running| running.what)
    }

    /// Rows the action list shows --- see [`BuildAction::list`].
    pub fn actions(&self, caps: &crate::backend::Capabilities) -> Vec<BuildAction> {
        BuildAction::list_for(caps, self.is_busy(), self.workspace_installed)
    }

    /// The action at `index` in the drawn list, mirroring the layout
    /// [`Self::actions`] describes.
    pub fn action_at(
        &self,
        caps: &crate::backend::Capabilities,
        index: usize,
    ) -> Option<BuildAction> {
        self.actions(caps).into_iter().nth(index)
    }

    /// The board name commands should target, whatever its origin.
    pub fn board_name(&self) -> Option<&str> {
        self.board.as_ref().map(|choice| choice.name.as_str())
    }

    /// The shield commands should configure for, when one is picked.
    pub fn shield_name(&self) -> Option<&str> {
        self.shield.as_deref()
    }

    /// Whether the lifecycle buttons can run: both checklist answers exist
    /// --- a buildable project root (`project_ok`, the caller's judgement,
    /// since buildability is a backend fact the panel stays agnostic
    /// about) and a board, picked or read from the build cache.
    pub fn lifecycle_ready(&self, project_ok: bool) -> bool {
        project_ok && self.board.is_some()
    }

    /// Applies a board chosen in the picker: it outranks the cache, and the
    /// app persists it in the project's registry entry so the next session
    /// starts from the same answer.
    pub fn set_picked(&mut self, name: impl Into<String>) {
        self.board = Some(BoardChoice {
            name: name.into(),
            origin: BoardOrigin::Picked,
        });
    }

    /// Applies the board saved in the project's registry entry: ranks like a
    /// pick for as long as the project stays the same (a cache read must not
    /// erase it), but is re-derived on a project switch.
    pub fn set_config_board(&mut self, name: impl Into<String>) {
        self.board = Some(BoardChoice {
            name: name.into(),
            origin: BoardOrigin::Config,
        });
    }

    /// Applies a shield answer --- picked, or loaded with the saved board.
    /// `None` (the picker's `(none)` row) builds without one.
    pub fn set_shield(&mut self, name: Option<String>) {
        self.shield = name;
    }

    /// The boards matching a picker filter: case-insensitive substring on
    /// the name or the description, order preserved (west sorts by name).
    pub fn filtered_boards<'a>(&'a self, filter: &str) -> Vec<&'a Board> {
        let filter = filter.to_lowercase();
        match &self.boards.state {
            ListState::Loaded(boards) => boards
                .iter()
                .filter(|board| {
                    matches_filter(&board.name, &board.description, &board.vendor, &filter)
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The shields matching a picker filter, same rule as the boards.
    pub fn filtered_shields<'a>(&'a self, filter: &str) -> Vec<&'a Shield> {
        let filter = filter.to_lowercase();
        match &self.shields.state {
            ListState::Loaded(shields) => shields
                .iter()
                .filter(|shield| {
                    matches_filter(&shield.name, &shield.description, &shield.vendor, &filter)
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// [`Self::filtered_boards`]'s count without collecting the list ---
    /// for callers (a click's row count) that never read the boards
    /// themselves, so a click no longer pays for a `Vec<&Board>` over
    /// Zephyr's 1000+-entry list just to discard it.
    pub fn filtered_boards_count(&self, filter: &str) -> usize {
        let filter = filter.to_lowercase();
        match &self.boards.state {
            ListState::Loaded(boards) => boards
                .iter()
                .filter(|board| {
                    matches_filter(&board.name, &board.description, &board.vendor, &filter)
                })
                .count(),
            _ => 0,
        }
    }

    /// [`Self::filtered_shields`]'s count, same trade as
    /// [`Self::filtered_boards_count`].
    pub fn filtered_shields_count(&self, filter: &str) -> usize {
        let filter = filter.to_lowercase();
        match &self.shields.state {
            ListState::Loaded(shields) => shields
                .iter()
                .filter(|shield| {
                    matches_filter(&shield.name, &shield.description, &shield.vendor, &filter)
                })
                .count(),
            _ => 0,
        }
    }

    /// Starts the background `west boards` fetch the picker needs. A no-op
    /// when one is already running or the list is already here: the fetch is
    /// once per session, like every other "load this eagerly, once" seam.
    pub fn start_boards_fetch(
        &mut self,
        command: crate::process::Command,
        processes: &mut ProcessManager,
    ) {
        self.boards.start(command, processes);
    }

    /// Starts the background `west shields` fetch, same rule as the boards.
    pub fn start_shields_fetch(
        &mut self,
        command: crate::process::Command,
        processes: &mut ProcessManager,
    ) {
        self.shields.start(command, processes);
    }

    /// The board-list command, rooted, tool- and environment-decorated like
    /// the build commands. `None` when the backend offers no board selection.
    pub fn boards_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.board_list_command(&self.board_roots)?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// The shield-list command, rooted and decorated like the others.
    /// `None` when the backend offers no shield selection.
    pub fn shields_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.shield_list_command(&self.board_roots)?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// Elapsed time of the running command, for the header's live counter.
    pub fn elapsed(&self) -> Option<Duration> {
        self.running
            .as_ref()
            .map(|running| running.started.elapsed())
    }

    /// The running command's latest parsed progress (see [`crate::progress`]),
    /// for the state line. `None` either before the first matching line
    /// arrives or once the command finishes.
    pub fn progress(&self) -> Option<crate::progress::Progress> {
        self.running.as_ref().and_then(|running| running.progress)
    }

    /// The command for `kind`, as the app should run it: the backend's
    /// construction, rooted at the project, pointed at the override tool if
    /// one is set. `None` means the backend offers no such operation.
    pub fn command(
        &self,
        kind: BuildKind,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.build_command(
            kind,
            self.build_board(),
            self.build_shield(),
            self.has_build_dir(),
            &self.build_dir,
        )?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// The flash command, rooted and decorated like the build ones, and
    /// always over the *board* variant's build directory
    /// ([`Self::flash_build_dir`]). `None` when the backend has no single
    /// flash command.
    pub fn flash_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.flash_command(&self.flash_build_dir())?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// The command that runs a simulator variant's host executable.
    ///
    /// Not `west`, and deliberately not `decorated`: this is the program the
    /// build produced, launched by its own path, and running it through the
    /// workspace's west would name a subcommand that does not exist. It
    /// still gets the west *environment* --- the executable is a Zephyr
    /// build and may read it --- and the project root as cwd, so a relative
    /// path in the application resolves the way it does under `west build`.
    ///
    /// `None` when the last build did not target a simulator, or its
    /// executable is not there: nothing offers this command directly, so a
    /// `None` here simply means the build that just finished produced no
    /// program to launch.
    pub fn run_command(&self) -> Option<crate::process::Command> {
        let variant = self.variant()?;
        if !variant.is_simulator() {
            return None;
        }
        let executable = variant.executable(&self.root);
        if !executable.is_file() {
            return None;
        }
        Some(
            self.in_west_env(crate::process::Command::new(
                executable.display().to_string(),
            ))
            .current_dir(&self.root),
        )
    }

    /// Consumes the "a simulator build just succeeded" flag, so the app
    /// can launch what it produced. Reading it clears it: the run happens
    /// once per build, not once per event drained afterwards.
    pub fn take_simulator_built(&mut self) -> bool {
        std::mem::take(&mut self.simulator_built)
    }

    /// The interactive configuration command (`menuconfig`), rooted and
    /// decorated like the others. The caller runs it with the terminal
    /// suspended --- it is not a piped process.
    pub fn menuconfig_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.menuconfig_command(&self.build_dir)?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// The build-dashboard command (`west build -t dashboard`), rooted and
    /// decorated like the others --- a piped command, streamed into the
    /// Monitor tab like the lifecycle.
    /// The memory-report command, decorated with this panel's cwd and
    /// environment like every other one it runs.
    ///
    /// `Err` is a refusal already phrased as a sentence --- the workspace's
    /// missing interpreter here, the backend's own reasons through it.
    pub fn size_report_command(
        &self,
        backend: &dyn crate::backend::Backend,
        workspace: &crate::backend::zephyr::workspace::Workspace,
    ) -> Result<crate::process::Command, String> {
        let python = workspace.python().ok_or_else(|| {
            "the workspace has no Python --- the memory report runs a script from the \
             Zephyr checkout, which needs the venv's interpreter"
                .to_string()
        })?;
        let paths = crate::backend::zephyr::report::ReportPaths::new(&self.root, &self.build_dir);
        let command = backend.size_report_command(&crate::backend::BuildReportContext {
            python: &python,
            zephyr_base: &workspace.zephyr_base,
            topdir: &workspace.dir,
            elf: &paths.elf(),
            out_dir: &paths.output,
        })?;
        // `size_report` opens its `--json` path with a plain `open(..., "w")`
        // and creates no parent, so a build directory that never ran Zephyr's
        // own `dashboard` target dies on a `FileNotFoundError` traceback after
        // the whole DWARF walk. Creating it here is the same write the target
        // itself would have made --- a build directory is regenerable output.
        if let Err(why) = std::fs::create_dir_all(&paths.output) {
            return Err(format!(
                "cannot create {} for the memory report: {why}",
                paths.output.display()
            ));
        }
        // Environment only: the program is the interpreter this command
        // already names, not the workspace's `west`.
        Ok(self.in_west_env(command.current_dir(&self.root)))
    }

    pub fn dashboard_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.dashboard_command(&self.build_dir)?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// Whether the lifecycle's build directory exists (configured by a
    /// previous build). The monitor asks the same fact: the platform
    /// monitor reads the build's runner configuration.
    pub fn has_build_dir(&self) -> bool {
        self.root.join(&self.build_dir).is_dir()
    }

    /// Starts `command` as this panel's running process. `what` labels it in
    /// the report line; `updates_board` marks commands whose success leaves
    /// a fresh CMakeCache behind; `action` is the row that was started, so
    /// `finish` can send the cursor back to it; `caps` shapes the list
    /// `Stop` lands on. `false` (without side effects) when something is
    /// already running --- one build at a time, same rule as the flash
    /// panel.
    pub fn start(
        &mut self,
        what: &'static str,
        updates_board: bool,
        action: BuildAction,
        command: crate::process::Command,
        processes: &mut ProcessManager,
        caps: &Capabilities,
    ) -> bool {
        if self.is_busy() {
            return false;
        }
        // The literal command leads the output: what streams below is only
        // meaningful attached to what produced it (`SPEC.md` §15's
        // never-hide-what-runs, applied to the log rather than a confirm).
        self.output.clear();
        self.output.push_back(format!("$ {command}"));
        let id = processes.spawn(command, BUILD_TIMEOUT);
        self.running = Some(Running {
            id,
            what,
            updates_board,
            action,
            started: Instant::now(),
            progress: None,
        });
        // `Stop` now trails the list, drawn as the half-width box in the
        // pane's bottom-right corner: land the cursor on it, so cancelling
        // is one Enter away.
        self.cursor = BuildAction::list_for(caps, true, self.workspace_installed).len() - 1;
        true
    }

    /// Points the cursor at `action` in the list the panel currently shows:
    /// the Clean → Build half of the lifecycle tour (a clean exists to be
    /// followed by a build, so that is where the cursor waits it out).
    /// A no-op when the list does not show the action.
    pub fn focus_action(&mut self, caps: &Capabilities, action: BuildAction) {
        if let Some(index) = BuildAction::list_for(caps, self.is_busy(), self.workspace_installed)
            .iter()
            .position(|candidate| *candidate == action)
        {
            self.cursor = index;
        }
    }

    /// Cancels the running command at the user's request.
    pub fn stop(&mut self, processes: &mut ProcessManager) -> bool {
        let Some(running) = &self.running else {
            return false;
        };
        processes.cancel(running.id);
        true
    }

    /// Whether a `Flash` command finished successfully since the last
    /// call: the built image was just written to the device, so whatever
    /// the firmware identification read before it is stale.
    pub fn take_size_report_finished(&mut self) -> bool {
        std::mem::take(&mut self.size_report_finished)
    }

    pub fn take_flash_finished(&mut self) -> bool {
        std::mem::take(&mut self.flash_finished)
    }

    /// Feeds a process event back into the panel, returning log notices.
    /// Covers all of its processes: the build command and the background
    /// `west boards`/`west shields` fetches --- each matched by id, each
    /// ignored when it is another's event. `caps` shapes the post-finish
    /// cursor target (Flash only exists under its capability).
    pub fn on_process(
        &mut self,
        event: &ProcessEvent,
        caps: &Capabilities,
    ) -> Vec<(Level, String)> {
        match event {
            // Raw PTY bytes belong to the Terminal tab's emulator alone;
            // build commands are piped.
            ProcessEvent::Bytes { .. } => Vec::new(),
            ProcessEvent::Line { id, text, .. } => {
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.id == *id)
                {
                    // The dashboard target runs its own cmake/ninja helpers
                    // whose `[0/1]` counters track an internal build, not
                    // the report being generated --- surfacing them as
                    // "Dashboard 0/1" would be noise, so the dashboard
                    // never adopts progress and its state line stays on
                    // the label (`ui::build::draw_state`).
                    let wants_progress = self.running.as_ref().is_none_or(|running| {
                        !matches!(
                            running.action,
                            BuildAction::Dashboard | BuildAction::SizeReport
                        )
                    });
                    if wants_progress
                        && let Some(progress) = crate::progress::detect(text)
                        && let Some(running) = &mut self.running
                    {
                        running.progress = Some(progress);
                    }
                    self.push_output(text.clone());
                } else {
                    self.boards.on_line(*id, text);
                    self.shields.on_line(*id, text);
                }
                Vec::new()
            }
            ProcessEvent::Finished {
                id,
                outcome,
                duration,
            } => {
                if self
                    .boards
                    .on_finished(*id, outcome, parse_boards, "board list", "west boards")
                    || self.shields.on_finished(
                        *id,
                        outcome,
                        parse_shields,
                        "shield list",
                        "west shields",
                    )
                {
                    return Vec::new();
                }
                let Some(running) = self.running.take() else {
                    return Vec::new();
                };
                if running.id != *id {
                    self.running = Some(running);
                    return Vec::new();
                }
                self.finish(running, outcome, *duration, caps)
            }
            ProcessEvent::Started { .. } | ProcessEvent::Output { .. } => Vec::new(),
        }
    }

    fn finish(
        &mut self,
        running: Running,
        outcome: &Outcome,
        duration: Duration,
        caps: &Capabilities,
    ) -> Vec<(Level, String)> {
        let ok = outcome.is_success();
        let what = running.what;
        let message = match outcome {
            Outcome::Success => format!("{what}: done in {}", Self::secs(duration)),
            Outcome::Failed { code } => match code {
                Some(code) => format!("{what} failed (exit code {code}) — see the Monitor tab"),
                None => format!("{what} was terminated by a signal — see the Monitor tab"),
            },
            Outcome::SpawnFailed(reason) => {
                format!("{what} could not start ({reason}) — is the backend's toolchain on PATH?")
            }
            Outcome::TimedOut => format!(
                "{what} did not finish within {} minutes",
                BUILD_TIMEOUT.as_secs() / 60
            ),
            Outcome::Cancelled => format!("{what} stopped"),
        };
        let level = if ok {
            Level::Success
        } else if matches!(outcome, Outcome::Cancelled) {
            // Neither a success nor an error --- the same third reading the
            // panel footer draws with its own mark. `stop_build` already
            // logged the request as a warning; answering it with a green
            // `Success` made one event read as two different things.
            Level::Info
        } else {
            Level::Error
        };

        self.last = Some(BuildReport {
            what: running.what,
            simulator: self.targets_simulator(),
            ok,
            cancelled: matches!(outcome, Outcome::Cancelled),
            duration,
            at: OffsetDateTime::now_utc().to_offset(self.offset),
        });
        // A finished build/rebuild leaves a fresh CMakeCache behind: re-read
        // it so the header (and the next `--pristine=always -b …`) tracks
        // what was just configured --- but a hand-picked board is session
        // state and is never demoted by a command: the pick is what the
        // commands pass as `-b`, and a cache that cannot be read (a layout
        // this reader does not know, a build that wrote elsewhere) must not
        // erase the user's explicit choice.
        // The list just lost its `Stop` tail, so a cursor still sitting on
        // it (parked there by `start`) would point past the end --- and
        // predate this command anyway, so it cannot say where to land.
        // Derive the target from the action that was actually started
        // instead: Flash after a successful build/rebuild (flash what was
        // just built), Build otherwise --- a retry, or, for Clean
        // specifically, the build it exists to clear the way for (matching
        // the `Build` row `start_build` parks the cursor on while a clean
        // runs); every other command (Flash, UpdateZephyr) lands back on
        // its own row.
        let settled = BuildAction::list(caps, false);
        let target = match running.action {
            BuildAction::Build(BuildKind::Clean) => BuildAction::Build(BuildKind::Build),
            // A host build has nothing to flash --- it is about to run
            // instead --- so the cursor stays on the row that would build
            // it again, which is the loop the user is actually in.
            BuildAction::Build(_) if ok && self.targets_simulator() => {
                BuildAction::Build(BuildKind::Build)
            }
            BuildAction::Build(_) if ok => BuildAction::Flash,
            BuildAction::Build(_) => BuildAction::Build(BuildKind::Build),
            // `Dashboard` has no row of its own (it is reached through the
            // Zephyr Actions menu), so it lands on the row that *opens* that
            // menu --- where the user was when they started it. Said out
            // loud because the fallback below would otherwise decide it:
            // `position` finds nothing for an unlisted action and
            // `unwrap_or(0)` happens to be this same row today, which is a
            // coincidence, not a decision.
            // Neither has a row of its own, so the cursor lands on the row
            // that opens the menu they were reached from.
            BuildAction::Dashboard | BuildAction::SizeReport => BuildAction::UpdateZephyr,
            // Also rowless: the run was started by the build that finished
            // just before it, so the cursor belongs where that build left
            // it --- on the row that would build again.
            BuildAction::Run => BuildAction::Build(BuildKind::Build),
            other => other,
        };
        self.cursor = settled
            .iter()
            .position(|candidate| *candidate == target)
            .unwrap_or(0);

        if ok && running.action == BuildAction::Flash {
            self.flash_finished = true;
        }
        if ok && running.action == BuildAction::SizeReport {
            self.size_report_finished = true;
        }
        if ok
            && matches!(
                running.action,
                BuildAction::Build(BuildKind::Build | BuildKind::Rebuild)
            )
            && self.targets_simulator()
        {
            self.simulator_built = true;
        }
        if ok && running.updates_board {
            let cached = cached_board(&self.root, &self.build_dir).map(|name| BoardChoice {
                name,
                origin: BoardOrigin::Cache,
            });
            if !matches!(
                self.board,
                Some(BoardChoice {
                    origin: BoardOrigin::Picked,
                    ..
                })
            ) {
                self.board = cached;
            }
        }
        vec![(level, message)]
    }

    /// "12.4s" / "2m 07s" --- durations a human compares, not milliseconds.
    pub fn secs(duration: Duration) -> String {
        let total = duration.as_secs_f32();
        if total < 60.0 {
            format!("{total:.1}s")
        } else {
            let minutes = (total / 60.0).floor() as u64;
            let seconds = duration.as_secs() % 60;
            format!("{minutes}m {seconds:02}s")
        }
    }

    fn push_output(&mut self, line: String) {
        if self.output.len() >= OUTPUT_CAPACITY {
            self.output.pop_front();
        }
        self.output.push_back(line);
    }
}

/// Reads the board a configured build directory targets, from the
/// `CACHED_BOARD:STRING=` entry of its CMake cache. The cache lives where
/// the generation roots the build system: classic builds at
/// `<dir>/zephyr/CMakeCache.txt`, sysbuild (the modern default) at the
/// top-level `<dir>/CMakeCache.txt` with per-image directories below it.
/// `None` when neither exists or neither holds the entry (a cache written
/// by something other than a Zephyr app build).
pub fn cached_board(root: &Path, build_dir: &str) -> Option<String> {
    cached_target(root, build_dir).map(|target| target.board)
}

/// The board *and* shield a configured build directory holds, from the same
/// two cache locations [`cached_board`] reads. `None` when neither cache
/// exists or neither names a board --- a shield without a board is not a
/// target, so the board is what makes the answer.
pub fn cached_target(root: &Path, build_dir: &str) -> Option<CachedTarget> {
    let build = root.join(build_dir);
    parse_cached_target(&build.join("zephyr/CMakeCache.txt"))
        .or_else(|| parse_cached_target(&build.join("CMakeCache.txt")))
}

fn parse_cached_target(cache: &Path) -> Option<CachedTarget> {
    let cache = std::fs::read_to_string(cache).ok()?;
    let value = |key: &str| {
        cache
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Some(CachedTarget {
        board: value(CACHED_BOARD_KEY)?,
        shield: value(CACHED_SHIELD_KEY),
    })
}

/// The picker filters' shared rule: everything when the filter is empty,
/// else a case-insensitive substring on the name, the description or the
/// vendor --- the vendor is not drawn on the row, but "lilygo" or "seeed"
/// is exactly how someone looks for their board.
fn matches_filter(name: &str, description: &str, vendor: &str, lowercase_filter: &str) -> bool {
    lowercase_filter.is_empty()
        || name.to_lowercase().contains(lowercase_filter)
        || description.to_lowercase().contains(lowercase_filter)
        || vendor.to_lowercase().contains(lowercase_filter)
}

/// Parses `west boards -f BOARD_FORMAT` output:
/// `name|qualifiers|vendor|full_name` per line, where `qualifiers` is
/// itself comma-separated and may be empty (a legacy board, or one whose
/// SoC has no cpuclusters).
///
/// **One line becomes one row per qualifier**, joined with `/`: the board
/// `ttgo_t_display_s3|esp32s3/procpu,esp32s3/appcpu|...` yields
/// `ttgo_t_display_s3/esp32s3/procpu` and `.../appcpu`, because those --- not
/// the bare name --- are the strings `west build -b` accepts. A board with
/// no qualifiers keeps its bare name, which is a valid target on its own.
///
/// The split is on `|` rather than whitespace: a `full_name` carries spaces
/// (`Native simulator - native_sim`), and the fields would otherwise run
/// into each other. A line without the separators is skipped rather than
/// fatal --- the list is 1400+ lines and one odd row must not empty it.
pub fn parse_boards(text: &str) -> Vec<Board> {
    let mut boards = Vec::new();
    for entry in parse_entries(text, 4) {
        let Ok([name, qualifiers, vendor, description]) = <[String; 4]>::try_from(entry) else {
            continue;
        };
        let targets: Vec<String> = qualifiers
            .split(',')
            .map(str::trim)
            .filter(|qualifier| !qualifier.is_empty())
            .map(|qualifier| format!("{name}/{qualifier}"))
            .collect();
        let targets = if targets.is_empty() {
            vec![name]
        } else {
            targets
        };
        boards.extend(targets.into_iter().map(|name| Board {
            name,
            description: description.clone(),
            vendor: vendor.clone(),
        }));
    }
    boards
}

/// Parses `west shields -f SHIELD_FORMAT` output:
/// `name|vendor|full_name` per line. The same `|`-separated shape the board
/// list uses, with no qualifier expansion --- a shield has none.
pub fn parse_shields(text: &str) -> Vec<Shield> {
    parse_entries(text, 3)
        .into_iter()
        .filter_map(|entry| {
            let [name, vendor, description] = <[String; 3]>::try_from(entry).ok()?;
            Some(Shield {
                name,
                description,
                vendor,
            })
        })
        .collect()
}

/// Splits each non-empty line into exactly `fields` `|`-separated, trimmed
/// parts. A line with the wrong arity is dropped: west emits the format
/// string verbatim, so a short line is a line that is not an entry (a
/// warning on stdout, say), never an entry to guess at.
fn parse_entries(text: &str, fields: usize) -> Vec<Vec<String>> {
    text.lines()
        .filter_map(|line| {
            let parts: Vec<String> = line
                .split('|')
                .map(|part| part.trim().to_string())
                .collect();
            (parts.len() == fields && !parts[0].is_empty()).then_some(parts)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::zephyr::ZephyrBackend;

    fn fixture_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chiptui-build-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("build/zephyr")).unwrap();
        dir
    }

    fn fake(tool: &str) -> String {
        format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn board_is_read_from_the_cmake_cache() {
        let dir = fixture_dir("cache");
        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "// cache\nCACHED_BOARD:STRING=nrf52840dk/nrf52840\nCACHED_APP:STRING=app\n",
        )
        .unwrap();
        assert_eq!(
            cached_board(&dir, DEFAULT_BUILD_DIR).as_deref(),
            Some("nrf52840dk/nrf52840")
        );
    }

    #[test]
    fn missing_or_boardless_cache_means_no_board() {
        let dir = fixture_dir("nocache");
        assert_eq!(cached_board(&dir, DEFAULT_BUILD_DIR), None);

        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=\n",
        )
        .unwrap();
        assert_eq!(
            cached_board(&dir, DEFAULT_BUILD_DIR),
            None,
            "an empty board is no board"
        );
    }

    #[test]
    fn the_board_is_read_from_a_sysbuild_top_level_cache_too() {
        let dir = fixture_dir("sysbuild");
        // Sysbuild roots the build system at build/ itself, with the
        // images' own caches below it --- the app's zephyr/ cache never
        // appears at the path the classic layout uses.
        std::fs::write(
            dir.join("build/CMakeCache.txt"),
            "CACHED_BOARD:STRING=nrf52840dk/nrf52840\n",
        )
        .unwrap();
        assert_eq!(
            cached_board(&dir, DEFAULT_BUILD_DIR).as_deref(),
            Some("nrf52840dk/nrf52840")
        );

        // When both spell a board, the classic entry wins: it is the
        // application's own configuration.
        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=thingy91/nrf9160\n",
        )
        .unwrap();
        assert_eq!(
            cached_board(&dir, DEFAULT_BUILD_DIR).as_deref(),
            Some("thingy91/nrf9160")
        );
    }

    #[test]
    fn a_picked_board_survives_a_finished_build() {
        let dir = fixture_dir("survive");
        // The fake west writes no CMakeCache: a build that leaves no cache
        // this reader can find must not erase the user's explicit pick.
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        panel.set_picked("nrf52840dk/nrf52840");

        let mut processes = ProcessManager::new();
        assert!(panel.start(
            BuildKind::Build.label(),
            true,
            BuildAction::Build(BuildKind::Build),
            crate::process::Command::new(fake("west")),
            &mut processes,
            &crate::backend::Capabilities::from_slice(&[crate::backend::Capability::Build]),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            for event in processes.drain() {
                panel.on_process(
                    &event,
                    &crate::backend::Capabilities::from_slice(&[crate::backend::Capability::Build]),
                );
            }
            if panel.last.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(panel.last.as_ref().is_some_and(|report| report.ok));
        assert_eq!(
            panel.board_name(),
            Some("nrf52840dk/nrf52840"),
            "a finished command must never erase a hand-picked board"
        );
        assert_eq!(panel.board.as_ref().unwrap().origin, BoardOrigin::Picked);
    }

    #[test]
    fn a_keep_command_lands_the_cursor_back_on_its_own_row() {
        // Regression: `start` always parks the cursor on the trailing
        // `Stop` slot, so a fallback that clamped that stale index into
        // the post-command list (rather than tracking the action that was
        // actually started) always resolved to the *last* row --- `Flash`,
        // a destructive action --- no matter which command had run.
        let dir = fixture_dir("keep-cursor");
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        // `west update` only exists as a row once an installation resolves.
        panel.workspace_installed = true;
        let caps = crate::backend::Capabilities::from_slice(&[
            crate::backend::Capability::Build,
            crate::backend::Capability::Clean,
            crate::backend::Capability::Flash,
            crate::backend::Capability::WorkspaceSync,
        ]);

        let mut processes = ProcessManager::new();
        assert!(panel.start(
            "Update Zephyr",
            false,
            BuildAction::UpdateZephyr,
            crate::process::Command::new(fake("west")),
            &mut processes,
            &caps,
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            for event in processes.drain() {
                panel.on_process(&event, &caps);
            }
            if panel.last.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(panel.last.as_ref().is_some_and(|report| report.ok));
        assert_eq!(
            panel.action_at(&caps, panel.cursor),
            Some(BuildAction::UpdateZephyr),
            "West update finishing must land back on its own row, not Flash"
        );
    }

    #[test]
    fn a_finished_clean_lands_the_cursor_on_build_not_flash() {
        // Regression: `finish`'s lifecycle-follow branch sent the cursor to
        // Flash after any successful lifecycle command, including Clean,
        // even though Clean's start-time code parks it on Build --- Clean
        // exists to clear the way for a build, never to be followed by a
        // flash of a build directory that was just wiped.
        let dir = fixture_dir("clean-cursor");
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        panel.set_picked("nrf52840dk/nrf52840");
        let caps = crate::backend::Capabilities::from_slice(&[
            crate::backend::Capability::Build,
            crate::backend::Capability::Clean,
            crate::backend::Capability::Flash,
        ]);

        let mut processes = ProcessManager::new();
        assert!(panel.start(
            BuildKind::Clean.label(),
            false,
            BuildAction::Build(BuildKind::Clean),
            crate::process::Command::new(fake("west")),
            &mut processes,
            &caps,
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            for event in processes.drain() {
                panel.on_process(&event, &caps);
            }
            if panel.last.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(panel.last.as_ref().is_some_and(|report| report.ok));
        assert_eq!(
            panel.action_at(&caps, panel.cursor),
            Some(BuildAction::Build(BuildKind::Build)),
            "a finished Clean must land on Build, not Flash"
        );
    }

    #[test]
    fn commands_carry_the_root_and_tool_override() {
        let dir = fixture_dir("cmd");
        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=nrf52840dk/nrf52840\n",
        )
        .unwrap();
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        panel.set_tool_path(fake("west"));

        let clean = panel.command(BuildKind::Clean, &ZephyrBackend).unwrap();
        assert_eq!(clean.program(), fake("west"));
        assert_eq!(clean.cwd(), Some(&dir));

        // Board from the cache, but the build directory exists, so the
        // incremental build never passes -b. (The display leads with the
        // fake's path: the tool override is the whole point.)
        let build = panel.command(BuildKind::Build, &ZephyrBackend).unwrap();
        assert!(build.to_string().ends_with("build"));
        assert!(!build.to_string().contains("-b"));
        let rebuild = panel.command(BuildKind::Rebuild, &ZephyrBackend).unwrap();
        assert!(rebuild.to_string().ends_with("-b nrf52840dk/nrf52840"));
    }

    #[test]
    fn the_tool_env_reaches_every_decorated_command() {
        let dir = fixture_dir("env");
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        panel.set_tool_env(vec![(
            "ZEPHYR_BASE".to_string(),
            dir.join("zephyr").display().to_string(),
        )]);

        let menuconfig = panel.menuconfig_command(&ZephyrBackend).unwrap();
        assert_eq!(menuconfig.to_string(), "west build -t menuconfig");
        assert_eq!(
            menuconfig.envs_slice(),
            [(
                "ZEPHYR_BASE".to_string(),
                dir.join("zephyr").display().to_string()
            )]
        );
        let boards = panel.boards_command(&ZephyrBackend).unwrap();
        assert!(boards.to_string().starts_with("west boards"));
    }

    #[test]
    fn another_build_dir_moves_the_lifecycle_and_the_board_answer() {
        let dir = fixture_dir("dirs");
        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=nrf52840dk/nrf52840\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("build-thingy/zephyr")).unwrap();
        std::fs::write(
            dir.join("build-thingy/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=thingy91/nrf9160\n",
        )
        .unwrap();
        // A sibling without a cache is not a build directory.
        std::fs::create_dir_all(dir.join("src")).unwrap();

        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        assert_eq!(panel.board_name(), Some("nrf52840dk/nrf52840"));

        panel.set_build_dir("build-thingy");
        assert_eq!(
            panel.board_name(),
            Some("thingy91/nrf9160"),
            "cache re-read per dir"
        );
        let build = panel.command(BuildKind::Build, &ZephyrBackend).unwrap();
        assert_eq!(build.to_string(), "west build -d build-thingy");
    }

    #[test]
    fn a_picked_board_survives_a_directory_switch() {
        let dir = fixture_dir("pickdir");
        std::fs::create_dir_all(dir.join("build/zephyr")).unwrap();
        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=old\n",
        )
        .unwrap();
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        panel.set_picked("nrf52840dk/nrf52840");

        panel.set_build_dir("build2");
        assert_eq!(
            panel.board_name(),
            Some("nrf52840dk/nrf52840"),
            "a session pick outlives directory switches"
        );
    }

    #[test]
    fn duration_reads_like_a_person() {
        assert_eq!(BuildPanel::secs(Duration::from_millis(12_400)), "12.4s");
        assert_eq!(BuildPanel::secs(Duration::from_secs(127)), "2m 07s");
    }

    #[test]
    fn west_boards_output_parses_into_targets() {
        let boards = parse_boards(
            "96b_carbon|stm32f407xx|96boards|96Boards Carbon\n\
             \n\
             nrf52840dk|nrf52840,nrf52840/qspi|nordic|Nordic nRF52840 DK\n\
             legacy_board|||\n\
             a bare name with no separators\n",
        );
        // One row per qualifier --- `west build -b` takes the qualified
        // target, never the bare name, for a board that has any.
        assert_eq!(boards.len(), 4);
        assert_eq!(boards[0].name, "96b_carbon/stm32f407xx");
        assert_eq!(boards[0].description, "96Boards Carbon");
        assert_eq!(boards[0].vendor, "96boards");
        assert_eq!(boards[1].name, "nrf52840dk/nrf52840");
        assert_eq!(boards[2].name, "nrf52840dk/nrf52840/qspi");
        // No qualifiers: the bare name *is* the target.
        assert_eq!(boards[3].name, "legacy_board");
        assert_eq!(boards[3].description, "");
    }

    /// The default `west boards` output --- bare names, which is what a
    /// caller that forgot `-f` gets --- carries none of the fields, so it
    /// yields nothing rather than a list of descriptionless half-targets
    /// that `west build -b` would reject.
    #[test]
    fn the_default_west_boards_shape_is_not_mistaken_for_a_target_list() {
        assert!(parse_boards("xiao_esp32c3\nnative_sim\n").is_empty());
    }

    /// A `full_name` carries spaces, so the fields cannot be split on
    /// whitespace --- the bug the `|` format exists to make impossible.
    #[test]
    fn a_description_with_spaces_survives_whole() {
        let boards =
            parse_boards("native_sim|native,native/64|zephyr|Native simulator - native_sim\n");
        assert_eq!(boards[0].name, "native_sim/native");
        assert_eq!(boards[1].name, "native_sim/native/64");
        assert_eq!(boards[1].description, "Native simulator - native_sim");
    }

    #[test]
    fn the_board_filter_matches_names_and_descriptions_case_insensitively() {
        let mut panel = BuildPanel::new("/nonexistent", UtcOffset::UTC);
        panel.boards.state = ListState::Loaded(parse_boards(
            "nrf52840dk|nrf52840|nordic|Nordic nRF52840 DK\nsting|||\n",
        ));

        assert_eq!(panel.filtered_boards("NRF52").len(), 1);
        assert_eq!(panel.filtered_boards("carbon").len(), 0);
        assert_eq!(panel.filtered_boards("st").len(), 1, "matches the name");
        assert_eq!(
            panel.filtered_boards("nordic").len(),
            1,
            "the vendor filters even though the row never draws it"
        );
        assert_eq!(
            panel.filtered_boards("").len(),
            2,
            "an empty filter shows everything"
        );
        // `filtered_boards_count` must never diverge from the list it
        // avoids collecting.
        for filter in ["NRF52", "carbon", "st", "nordic", ""] {
            assert_eq!(
                panel.filtered_boards_count(filter),
                panel.filtered_boards(filter).len()
            );
        }
    }

    #[test]
    fn west_shields_output_parses_and_filters_like_boards() {
        let mut panel = BuildPanel::new("/nonexistent", UtcOffset::UTC);
        panel.shields.state = ListState::Loaded(parse_shields(
            "link_board_eth|wiznet|WIZnet W5500 Ethernet Shield\nnrf7002ek||\n",
        ));
        assert_eq!(panel.shields.state, {
            ListState::Loaded(vec![
                Shield {
                    name: "link_board_eth".to_string(),
                    description: "WIZnet W5500 Ethernet Shield".to_string(),
                    vendor: "wiznet".to_string(),
                },
                Shield {
                    name: "nrf7002ek".to_string(),
                    description: String::new(),
                    vendor: String::new(),
                },
            ])
        });
        assert_eq!(panel.filtered_shields("eth").len(), 1);
        assert_eq!(panel.filtered_shields("wiznet").len(), 1);
        assert_eq!(panel.filtered_shields("").len(), 2);
        // Same equivalence as the boards count.
        for filter in ["eth", "wiznet", ""] {
            assert_eq!(
                panel.filtered_shields_count(filter),
                panel.filtered_shields(filter).len()
            );
        }
    }

    #[test]
    fn a_picked_shield_reaches_the_lifecycle_and_none_clears_it() {
        // No build directory: the lifecycle is the first configuration,
        // where the shield (like the board) is a flag the command carries.
        let dir = fixture_dir("shield");
        let _ = std::fs::remove_dir_all(dir.join("build"));
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        assert_eq!(panel.shield_name(), None);
        // The shield never gates the lifecycle: it is optional, and `None`
        // ("no shield") is as valid an answer as a name.
        assert!(!panel.lifecycle_ready(false));

        panel.set_shield(Some("nrf7002ek".to_string()));
        let build = panel.command(BuildKind::Build, &ZephyrBackend).unwrap();
        assert_eq!(build.to_string(), "west build --shield nrf7002ek");
        let rebuild = panel.command(BuildKind::Rebuild, &ZephyrBackend).unwrap();
        assert_eq!(
            rebuild.to_string(),
            "west build --pristine=always --shield nrf7002ek"
        );

        panel.set_shield(None);
        let build = panel.command(BuildKind::Build, &ZephyrBackend).unwrap();
        assert_eq!(
            build.to_string(),
            "west build",
            "no shield is no flag at all"
        );
    }

    #[test]
    fn workspace_sync_leads_the_buttons_and_stop_trails_them_when_running() {
        let mut panel = BuildPanel::new("/nonexistent", UtcOffset::UTC);
        // With an installation resolved --- which is what makes the first
        // row `Update Zephyr` rather than the `Install Zephyr` that shares
        // its slot.
        panel.workspace_installed = true;
        // Zephyr's real set: `west update`, menuconfig, the lifecycle,
        // flash --- the project/board questions live in the workspace pane.
        let zephyr = crate::backend::Capabilities::from_slice(&[
            crate::backend::Capability::Build,
            crate::backend::Capability::Clean,
            crate::backend::Capability::Flash,
            crate::backend::Capability::BoardSelect,
            crate::backend::Capability::ProjectSelect,
            crate::backend::Capability::WorkspaceSync,
        ]);
        assert_eq!(
            panel.actions(&zephyr),
            vec![
                BuildAction::UpdateZephyr,
                BuildAction::Menuconfig,
                BuildAction::Build(BuildKind::Clean),
                BuildAction::Build(BuildKind::Build),
                BuildAction::Build(BuildKind::Rebuild),
                BuildAction::Flash,
            ]
        );
        assert_eq!(panel.action_at(&zephyr, 0), Some(BuildAction::UpdateZephyr));
        assert_eq!(panel.action_at(&zephyr, 5), Some(BuildAction::Flash));

        // A backend without flash or workspace sync: menuconfig and the
        // lifecycle alone.
        let plain = crate::backend::Capabilities::from_slice(&[crate::backend::Capability::Build]);
        assert_eq!(
            panel.actions(&plain),
            vec![
                BuildAction::Menuconfig,
                BuildAction::Build(BuildKind::Clean),
                BuildAction::Build(BuildKind::Build),
                BuildAction::Build(BuildKind::Rebuild),
            ]
        );

        // The lifecycle needs both answers: a boardless project and a
        // projectless board each leave the buttons disabled.
        assert!(!panel.lifecycle_ready(true), "no board yet");
        assert!(!panel.lifecycle_ready(false), "no project yet");
        panel.set_picked("nrf52840dk/nrf52840");
        assert!(panel.lifecycle_ready(true));

        // With a command running, Stop is appended --- the cursor lands on
        // it (the row the key handler's Enter cancels), the buttons above
        // keep their indices.
        let mut processes = ProcessManager::new();
        panel.start(
            BuildKind::Build.label(),
            true,
            BuildAction::Build(BuildKind::Build),
            crate::process::Command::new(fake("west")),
            &mut processes,
            &zephyr,
        );
        let running = panel.actions(&zephyr);
        assert_eq!(running.last(), Some(&BuildAction::Stop));
        assert_eq!(
            panel.action_at(&zephyr, panel.cursor),
            Some(BuildAction::Stop),
            "a started command parks the cursor on Stop"
        );
        assert_eq!(
            panel.action_at(&zephyr, 4),
            Some(BuildAction::Build(BuildKind::Rebuild))
        );
    }

    #[test]
    fn a_picked_board_outranks_the_cache() {
        let dir = fixture_dir("picked");
        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=old_board\n",
        )
        .unwrap();
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        assert_eq!(panel.board_name(), Some("old_board"));

        panel.set_picked("nrf52840dk/nrf52840");
        assert_eq!(panel.board_name(), Some("nrf52840dk/nrf52840"));
        assert_eq!(panel.board.as_ref().unwrap().origin, BoardOrigin::Picked);

        // The pick reaches the commands: rebuild always passes the target.
        let rebuild = panel.command(BuildKind::Rebuild, &ZephyrBackend).unwrap();
        assert!(rebuild.to_string().ends_with("-b nrf52840dk/nrf52840"));
    }

    #[test]
    fn a_picked_project_reroots_the_lifecycle_and_resets_project_facts() {
        let dir = fixture_dir("setproject");
        std::fs::create_dir_all(dir.join("build/zephyr")).unwrap();
        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=old_board\n",
        )
        .unwrap();
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        assert_eq!(panel.project_origin, ProjectOrigin::WorkingDir);
        panel.set_build_dir("build-custom");

        // The picked project: its own cached board, its own build dirs.
        let other = dir.join("other-app");
        std::fs::create_dir_all(other.join("build/zephyr")).unwrap();
        std::fs::write(
            other.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=thingy91/nrf9160\n",
        )
        .unwrap();
        panel.set_project(&other);

        assert_eq!(panel.root, other, "commands run in the picked project");
        assert_eq!(panel.project_origin, ProjectOrigin::Picked);
        assert_eq!(
            panel.build_dir, DEFAULT_BUILD_DIR,
            "the old dir was the other project's"
        );
        assert_eq!(
            panel.board_name(),
            Some("thingy91/nrf9160"),
            "the cache re-read belongs to the new project"
        );
        assert_eq!(panel.last, None, "the old report described another project");
        // The lifecycle reflects the new root: its build/ exists, so the
        // incremental shape (no `-b`) is the right one.
        assert_eq!(
            panel
                .command(BuildKind::Build, &ZephyrBackend)
                .unwrap()
                .to_string(),
            "west build"
        );
    }

    #[test]
    fn a_hand_picked_board_survives_a_project_switch() {
        let dir = fixture_dir("projboard");
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        panel.set_picked("nrf52840dk/nrf52840");
        panel.set_project(dir.join("elsewhere"));
        assert_eq!(
            panel.board_name(),
            Some("nrf52840dk/nrf52840"),
            "a board pick outlives project switches, like directory switches"
        );
    }

    #[test]
    fn a_saved_board_outranks_the_cache_and_survives_a_build_dir_switch() {
        let dir = fixture_dir("configboard");
        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=old_board\n",
        )
        .unwrap();
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        assert_eq!(panel.board_name(), Some("old_board"));

        // The registry answer is applied on open: same rank as a pick.
        panel.set_config_board("nrf52840dk/nrf52840");
        assert_eq!(panel.board_name(), Some("nrf52840dk/nrf52840"));
        assert_eq!(panel.board.as_ref().unwrap().origin, BoardOrigin::Config);

        // Cache re-reads (a build-directory switch) must not erase it.
        panel.set_build_dir("build-custom");
        assert_eq!(
            panel.board_name(),
            Some("nrf52840dk/nrf52840"),
            "a saved board outlives cache refreshes, like a pick"
        );
    }

    #[test]
    fn a_saved_board_and_shield_reset_on_a_project_switch() {
        let dir = fixture_dir("projconfig");
        let mut panel = BuildPanel::new(&dir, UtcOffset::UTC);
        panel.set_config_board("nrf52840dk/nrf52840");
        panel.set_shield(Some("nrf7002ek".to_string()));

        // The picked project: its own cached board answers instead.
        let other = dir.join("other-app");
        std::fs::create_dir_all(other.join("build/zephyr")).unwrap();
        std::fs::write(
            other.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=thingy91/nrf9160\n",
        )
        .unwrap();
        panel.set_project(&other);

        assert_eq!(
            panel.board_name(),
            Some("thingy91/nrf9160"),
            "a saved board belongs to the project being left, not the session"
        );
        assert_eq!(
            panel.shield_name(),
            None,
            "the saved shield is project-scoped the same way"
        );
    }
}
