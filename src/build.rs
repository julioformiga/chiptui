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

/// Result of the last finished command, for the panel's header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildReport {
    /// What ran, as a label ("Build", "Clean", "Flash", …).
    pub what: &'static str,
    pub ok: bool,
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
}

/// One board target from `west boards`: the name `west build -b` takes and
/// the human description shown beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub name: String,
    pub description: String,
}

/// One shield from `west shields`: the name `west build --shield` takes and
/// the human description shown beside it. The same `name description` line
/// shape as the board list, which is why the parse is shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shield {
    pub name: String,
    pub description: String,
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
/// `UpdateZephyr`/`SdkList` are gated on the resolved workspace instead
/// (`App::build_action_enabled`), since they act on the shared installation,
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
    /// `west sdk list` --- the toolchain inventory. Read-only, runs
    /// immediately like the lifecycle's non-destructive rows.
    SdkList,
}

impl BuildAction {
    /// Rows the list shows under `caps`: the workspace-scoped pair first
    /// (`west update`, `west sdk list` --- the environment every later
    /// action runs in), then menuconfig (a build-system question answered
    /// before any artifact exists), then the lifecycle in its own order
    /// (clean, build, rebuild), then flash under its capability. With
    /// `running`, `Stop` is appended (cancelling as discoverable as
    /// starting, `SPEC.md` §12); the pane draws it as its own half-width
    /// box in the bottom-right corner rather than a row of the stack.
    /// The lifecycle targets the conventional `build` directory inside the
    /// project --- no directory picker.
    pub fn list(caps: &Capabilities, running: bool) -> Vec<Self> {
        let mut actions = Vec::with_capacity(BuildKind::ALL.len() + 5);
        if caps.contains(crate::backend::Capability::WorkspaceSync) {
            actions.push(Self::UpdateZephyr);
            actions.push(Self::SdkList);
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
            root,
            build_dir: DEFAULT_BUILD_DIR.to_string(),
            board,
            shield: None,
            project_origin: ProjectOrigin::default(),
            cursor: 0,
            last: None,
            output: VecDeque::new(),
            boards: ListFetch::default(),
            shields: ListFetch::default(),
            offset,
            running: None,
            flash_finished: false,
            tool_path: None,
            tool_env: Vec::new(),
        }
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
    fn decorated(&self, command: crate::process::Command) -> crate::process::Command {
        let command = match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        };
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
        self.last = None;
        self.cursor = 0;
    }

    /// The project's configured build directories: immediate subdirectories
    /// holding a `zephyr/CMakeCache.txt` (west's own footprint), `build`
    /// first when present. What the build-directory picker lists; an empty
    /// answer simply means nothing is configured yet.
    pub fn discover_build_dirs(root: &Path) -> Vec<String> {
        let mut dirs: Vec<String> = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().join("zephyr/CMakeCache.txt").is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
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
        BuildAction::list(caps, self.is_busy())
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
                .filter(|board| matches_filter(&board.name, &board.description, &filter))
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
                .filter(|shield| matches_filter(&shield.name, &shield.description, &filter))
                .collect(),
            _ => Vec::new(),
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
        let command = backend.board_list_command()?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// The shield-list command, rooted and decorated like the others.
    /// `None` when the backend offers no shield selection.
    pub fn shields_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.shield_list_command()?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// Elapsed time of the running command, for the header's live counter.
    pub fn elapsed(&self) -> Option<Duration> {
        self.running
            .as_ref()
            .map(|running| running.started.elapsed())
    }

    /// The build-directory picker's rows for a filter: the typed name first
    /// when it would create something new, then the conventional `build`,
    /// then every other configured directory --- so even a fresh project
    /// (nothing configured yet) has the default to fall back on. A name is
    /// only ever a *name*: path separators and `..` never qualify (the
    /// directory lives inside the project root, like `west`'s own `-d`
    /// argument).
    pub fn filtered_build_dirs(&self, filter: &str) -> Vec<String> {
        let filter = filter.trim();
        if !filter.is_empty() && !Self::is_build_dir_name(filter) {
            return Vec::new();
        }
        let mut dirs: Vec<String> = Self::discover_build_dirs(&self.root)
            .into_iter()
            .filter(|dir| filter.is_empty() || dir.contains(filter))
            .collect();
        // A filter that matches nothing is a name being typed for a new
        // directory: it leads the list (first Enter lands on it).
        if !filter.is_empty() && dirs.is_empty() {
            dirs.push(filter.to_string());
        }
        if (filter.is_empty() || DEFAULT_BUILD_DIR.contains(filter))
            && !dirs.contains(&DEFAULT_BUILD_DIR.to_string())
        {
            dirs.insert(0, DEFAULT_BUILD_DIR.to_string());
        }
        dirs
    }

    /// A legal build-directory name: a single path component.
    fn is_build_dir_name(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains(std::path::MAIN_SEPARATOR_STR)
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
            self.board_name(),
            self.shield_name(),
            self.has_build_dir(),
            &self.build_dir,
        )?;
        Some(self.decorated(command.current_dir(&self.root)))
    }

    /// The flash command, rooted and decorated like the build ones.
    /// `None` when the backend has no single flash command.
    pub fn flash_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.flash_command(&self.build_dir)?;
        Some(self.decorated(command.current_dir(&self.root)))
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

    fn has_build_dir(&self) -> bool {
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
        });
        // `Stop` now trails the list, drawn as the half-width box in the
        // pane's bottom-right corner: land the cursor on it, so cancelling
        // is one Enter away.
        self.cursor = BuildAction::list(caps, true).len() - 1;
        true
    }

    /// Points the cursor at `action` in the list the panel currently shows:
    /// the Clean → Build half of the lifecycle tour (a clean exists to be
    /// followed by a build, so that is where the cursor waits it out).
    /// A no-op when the list does not show the action.
    pub fn focus_action(&mut self, caps: &Capabilities, action: BuildAction) {
        if let Some(index) = BuildAction::list(caps, self.is_busy())
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
            ProcessEvent::Line { id, text, .. } => {
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.id == *id)
                {
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
            Outcome::Cancelled => format!("{what}: cancelled"),
        };
        let level = if ok {
            Level::Success
        } else if matches!(outcome, Outcome::Cancelled) {
            Level::Warn
        } else {
            Level::Error
        };

        self.last = Some(BuildReport {
            what: running.what,
            ok,
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
        // runs); every other command (Flash, UpdateZephyr, SdkList) lands
        // back on its own row.
        let settled = BuildAction::list(caps, false);
        let target = match running.action {
            BuildAction::Build(BuildKind::Clean) => BuildAction::Build(BuildKind::Build),
            BuildAction::Build(_) if ok => BuildAction::Flash,
            BuildAction::Build(_) => BuildAction::Build(BuildKind::Build),
            other => other,
        };
        self.cursor = settled
            .iter()
            .position(|candidate| *candidate == target)
            .unwrap_or(0);

        if ok && running.action == BuildAction::Flash {
            self.flash_finished = true;
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
    let build = root.join(build_dir);
    parse_cached_board(&build.join("zephyr/CMakeCache.txt"))
        .or_else(|| parse_cached_board(&build.join("CMakeCache.txt")))
}

fn parse_cached_board(cache: &Path) -> Option<String> {
    let cache = std::fs::read_to_string(cache).ok()?;
    cache
        .lines()
        .find_map(|line| line.strip_prefix(CACHED_BOARD_KEY))
        .map(str::trim)
        .filter(|board| !board.is_empty())
        .map(str::to_string)
}

/// The picker filters' shared rule: everything when the filter is empty,
/// else a case-insensitive substring on the name or the description.
fn matches_filter(name: &str, description: &str, lowercase_filter: &str) -> bool {
    lowercase_filter.is_empty()
        || name.to_lowercase().contains(lowercase_filter)
        || description.to_lowercase().contains(lowercase_filter)
}

/// Parses `west boards` output: one target per line as `name description`
/// (description optional; HWMv2 names carry a `/`). Blank and non-matching
/// lines are skipped rather than fatal --- the list is hundreds of lines
/// long and one odd row must not empty it.
pub fn parse_boards(text: &str) -> Vec<Board> {
    parse_entries(text, |name, description| Board { name, description })
}

/// Parses `west shields` output: the same one-entry-per-line
/// `name description` shape `west boards` prints, so the entries share the
/// parse and differ only in what they feed.
pub fn parse_shields(text: &str) -> Vec<Shield> {
    parse_entries(text, |name, description| Shield { name, description })
}

fn parse_entries<T>(text: &str, make: impl Fn(String, String) -> T) -> Vec<T> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let description = parts.collect::<Vec<_>>().join(" ");
            Some(make(name.to_string(), description))
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

        assert_eq!(
            BuildPanel::discover_build_dirs(&dir),
            vec!["build", "build-thingy"],
            "only configured directories count, sorted"
        );
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
    fn the_dir_picker_offers_the_default_and_a_new_typed_name() {
        let dir = fixture_dir("picker");
        std::fs::create_dir_all(dir.join("build-nrf/zephyr")).unwrap();
        std::fs::write(
            dir.join("build-nrf/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=nrf\n",
        )
        .unwrap();
        let panel = BuildPanel::new(&dir, UtcOffset::UTC);

        // Empty filter: the conventional default plus what is configured.
        assert_eq!(
            panel.filtered_build_dirs(""),
            vec![DEFAULT_BUILD_DIR.to_string(), "build-nrf".to_string()]
        );

        // A new name leads the list (first Enter press lands on it); an
        // existing name filters to itself.
        assert_eq!(
            panel.filtered_build_dirs("build-91"),
            vec!["build-91".to_string()]
        );
        assert_eq!(
            panel.filtered_build_dirs("nrf"),
            vec!["build-nrf".to_string()]
        );

        // Not a name: no rows, so Enter cannot apply it.
        assert!(panel.filtered_build_dirs("../escape").is_empty());
        assert!(panel.filtered_build_dirs("a/b").is_empty());
    }

    #[test]
    fn duration_reads_like_a_person() {
        assert_eq!(BuildPanel::secs(Duration::from_millis(12_400)), "12.4s");
        assert_eq!(BuildPanel::secs(Duration::from_secs(127)), "2m 07s");
    }

    #[test]
    fn west_boards_output_parses_into_targets() {
        let boards = parse_boards(
            "96b_carbon                   96Boards Carbon (STM32F407VE)\n\
             \n\
             nrf52840dk/nrf52840          Nordic nRF52840 DK\n\
             bare_name\n",
        );
        assert_eq!(boards.len(), 3);
        assert_eq!(boards[0].name, "96b_carbon");
        assert_eq!(boards[0].description, "96Boards Carbon (STM32F407VE)");
        // HWMv2 qualifiers stay part of the name; a name-only line is a
        // target with an empty description, not an error.
        assert_eq!(boards[1].name, "nrf52840dk/nrf52840");
        assert_eq!(boards[2].description, "");
    }

    #[test]
    fn the_board_filter_matches_names_and_descriptions_case_insensitively() {
        let mut panel = BuildPanel::new("/nonexistent", UtcOffset::UTC);
        panel.boards.state = ListState::Loaded(parse_boards(
            "nrf52840dk/nrf52840  Nordic nRF52840 DK\nsting\n",
        ));

        assert_eq!(panel.filtered_boards("NRF52").len(), 1);
        assert_eq!(panel.filtered_boards("carbon").len(), 0);
        assert_eq!(panel.filtered_boards("st").len(), 1, "matches the name");
        assert_eq!(
            panel.filtered_boards("").len(),
            2,
            "an empty filter shows everything"
        );
    }

    #[test]
    fn west_shields_output_parses_and_filters_like_boards() {
        let mut panel = BuildPanel::new("/nonexistent", UtcOffset::UTC);
        panel.shields.state = ListState::Loaded(parse_shields(
            "link_board_eth  WIZnet W5500 Ethernet Shield\nnrf7002ek\n",
        ));
        assert_eq!(panel.shields.state, {
            ListState::Loaded(vec![
                Shield {
                    name: "link_board_eth".to_string(),
                    description: "WIZnet W5500 Ethernet Shield".to_string(),
                },
                Shield {
                    name: "nrf7002ek".to_string(),
                    description: String::new(),
                },
            ])
        });
        assert_eq!(panel.filtered_shields("eth").len(), 1);
        assert_eq!(panel.filtered_shields("wiznet").len(), 1);
        assert_eq!(panel.filtered_shields("").len(), 2);
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
    fn the_workspace_pair_leads_the_buttons_and_stop_trails_them_when_running() {
        let mut panel = BuildPanel::new("/nonexistent", UtcOffset::UTC);
        // Zephyr's real set: the workspace pair, menuconfig, the lifecycle,
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
                BuildAction::SdkList,
                BuildAction::Menuconfig,
                BuildAction::Build(BuildKind::Clean),
                BuildAction::Build(BuildKind::Build),
                BuildAction::Build(BuildKind::Rebuild),
                BuildAction::Flash,
            ]
        );
        assert_eq!(panel.action_at(&zephyr, 0), Some(BuildAction::UpdateZephyr));
        assert_eq!(panel.action_at(&zephyr, 6), Some(BuildAction::Flash));

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
            panel.action_at(&zephyr, 5),
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
