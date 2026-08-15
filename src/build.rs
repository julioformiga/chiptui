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

use crate::backend::BuildKind;
use crate::logs::Level;
use crate::process::{Outcome, ProcessEvent, ProcessId, ProcessManager};

/// A Zephyr build from scratch is minutes, not seconds (`FLASH_TIMEOUT`'s
/// 180s would kill a legitimate first build). Half an hour accommodates a
/// cold `west` workspace without letting a wedged compiler live forever.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `west boards` walks every board root; two minutes covers a full Zephyr
/// SDK checkout without letting a wedged west live forever.
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
    started: Instant,
}

/// One board target from `west boards`: the name `west build -b` takes and
/// the human description shown beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub name: String,
    pub description: String,
}

/// Where the panel's board answer came from --- the header's one-word hint,
/// and the guard that keeps a cache read from silently erasing a user pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardOrigin {
    /// Read from `build/zephyr/CMakeCache.txt`.
    Cache,
    /// Chosen in the board picker, for this session only: nothing is
    /// written to the project (`SPEC.md` §10).
    Picked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardChoice {
    pub name: String,
    pub origin: BoardOrigin,
}

/// The board list's lifecycle: `west boards` is slow, so it is fetched in
/// the background the first time the picker opens and kept afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BoardsState {
    #[default]
    Idle,
    Loading,
    Loaded(Vec<Board>),
    Failed(String),
}

/// One row of the build panel's action list. `Stop`/`Board` bookend the
/// [`BuildKind`] entries exactly when they apply (see
/// [`BuildPanel::actions`]); `Flash` sits between them under
/// [`crate::backend::Capability::Flash`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildAction {
    Stop,
    Build(BuildKind),
    /// Writes the built image to the device --- destructive (`SPEC.md` §15),
    /// so it always routes through the confirm overlay.
    Flash,
    /// Opens the board picker (only under [`crate::backend::Capability::
    /// BoardSelect`]).
    Board,
}

impl BuildAction {
    /// Rows for an idle panel under `caps`: the build lifecycle, then flash
    /// and board when the capabilities allow. With `running`, `Stop` is
    /// prepended (cancelling as discoverable as starting, `SPEC.md` §12).
    pub fn list(caps: &crate::backend::Capabilities, running: bool) -> Vec<Self> {
        let mut actions = Vec::with_capacity(BuildKind::ALL.len() + 3);
        if running {
            actions.push(Self::Stop);
        }
        actions.extend(BuildKind::ALL.iter().map(|kind| Self::Build(*kind)));
        if caps.contains(crate::backend::Capability::Flash) {
            actions.push(Self::Flash);
        }
        if caps.contains(crate::backend::Capability::BoardSelect) {
            actions.push(Self::Board);
        }
        actions
    }
}

pub struct BuildPanel {
    /// Project root: the working directory every command runs in (`west`
    /// finds its workspace from the cwd).
    pub root: PathBuf,
    /// The board commands target: from the CMake cache until the user picks
    /// one for the session (a pick survives, and overrides, cache reads).
    pub board: Option<BoardChoice>,
    pub cursor: usize,
    pub last: Option<BuildReport>,
    pub output: VecDeque<String>,
    /// The `west boards` list and its fetch state, for the picker.
    pub boards: BoardsState,
    boards_process: Option<ProcessId>,
    boards_output: String,
    offset: UtcOffset,
    running: Option<Running>,
    /// Overrides the executable the backend's commands name (`west`), the
    /// seam tests (and later `[tools]` config) plug a substitute into.
    tool_path: Option<String>,
}

impl BuildPanel {
    pub fn new(root: impl Into<PathBuf>, offset: UtcOffset) -> Self {
        let root = root.into();
        let board = cached_board(&root).map(|name| BoardChoice {
            name,
            origin: BoardOrigin::Cache,
        });
        Self {
            root,
            board,
            cursor: 0,
            last: None,
            output: VecDeque::new(),
            boards: BoardsState::Idle,
            boards_process: None,
            boards_output: String::new(),
            offset,
            running: None,
            tool_path: None,
        }
    }

    pub fn set_tool_path(&mut self, program: impl Into<String>) {
        self.tool_path = Some(program.into());
    }

    pub fn tool_path(&self) -> Option<&str> {
        self.tool_path.as_deref()
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some()
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

    /// Applies a board chosen in the picker: session-only, never written to
    /// the project (`SPEC.md` §10) --- which is exactly why it outranks the
    /// cache until the session ends.
    pub fn set_picked(&mut self, name: impl Into<String>) {
        self.board = Some(BoardChoice {
            name: name.into(),
            origin: BoardOrigin::Picked,
        });
    }

    /// The boards matching a picker filter: case-insensitive substring on
    /// the name or the description, order preserved (west sorts by name).
    pub fn filtered_boards<'a>(&'a self, filter: &str) -> Vec<&'a Board> {
        let filter = filter.to_lowercase();
        match &self.boards {
            BoardsState::Loaded(boards) => boards
                .iter()
                .filter(|board| {
                    filter.is_empty()
                        || board.name.to_lowercase().contains(&filter)
                        || board.description.to_lowercase().contains(&filter)
                })
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
        if self.boards_process.is_some() || matches!(self.boards, BoardsState::Loaded(_)) {
            return;
        }
        self.boards = BoardsState::Loading;
        self.boards_output.clear();
        self.boards_process = Some(processes.spawn(command, BOARDS_TIMEOUT));
    }

    /// The board-list command, rooted and tool-overridden like the build
    /// commands. `None` when the backend offers no board selection.
    pub fn boards_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.board_list_command()?;
        let command = command.current_dir(&self.root);
        Some(match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        })
    }

    /// Elapsed time of the running command, for the header's live counter.
    pub fn elapsed(&self) -> Option<Duration> {
        self.running
            .as_ref()
            .map(|running| running.started.elapsed())
    }

    /// The command for `kind`, as the app should run it: the backend's
    /// construction, rooted at the project, pointed at the override tool if
    /// one is set. `None` means the backend offers no such operation.
    pub fn command(
        &self,
        kind: BuildKind,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.build_command(kind, self.board_name(), self.has_build_dir())?;
        let command = command.current_dir(&self.root);
        Some(match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        })
    }

    /// The flash command, rooted and tool-overridden like the build ones.
    /// `None` when the backend has no single flash command.
    pub fn flash_command(
        &self,
        backend: &dyn crate::backend::Backend,
    ) -> Option<crate::process::Command> {
        let command = backend.flash_command()?;
        let command = command.current_dir(&self.root);
        Some(match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        })
    }

    fn has_build_dir(&self) -> bool {
        self.root.join("build").is_dir()
    }

    /// Starts `command` as this panel's running process. `what` labels it in
    /// the report line; `updates_board` marks commands whose success leaves
    /// a fresh CMakeCache behind. `false` (without side effects) when
    /// something is already running --- one build at a time, same rule as
    /// the flash panel.
    pub fn start(
        &mut self,
        what: &'static str,
        updates_board: bool,
        command: crate::process::Command,
        processes: &mut ProcessManager,
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
            started: Instant::now(),
        });
        true
    }

    /// Cancels the running command at the user's request.
    pub fn stop(&mut self, processes: &mut ProcessManager) -> bool {
        let Some(running) = &self.running else {
            return false;
        };
        processes.cancel(running.id);
        true
    }

    /// Feeds a process event back into the panel, returning log notices.
    /// Covers both of its processes: the build command and the background
    /// `west boards` fetch --- each matched by id, each ignored when it is
    /// the other's event.
    pub fn on_process(&mut self, event: &ProcessEvent) -> Vec<(Level, String)> {
        match event {
            ProcessEvent::Line { id, text, .. } => {
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.id == *id)
                {
                    self.push_output(text.clone());
                } else if self.boards_process == Some(*id) {
                    self.boards_output.push_str(text);
                    self.boards_output.push('\n');
                }
                Vec::new()
            }
            ProcessEvent::Finished {
                id,
                outcome,
                duration,
            } => {
                if self.boards_process == Some(*id) {
                    self.finish_boards_fetch(outcome);
                    return Vec::new();
                }
                let Some(running) = self.running.take() else {
                    return Vec::new();
                };
                if running.id != *id {
                    self.running = Some(running);
                    return Vec::new();
                }
                self.finish(running, outcome, *duration)
            }
            ProcessEvent::Started { .. } | ProcessEvent::Output { .. } => Vec::new(),
        }
    }

    /// Parses the accumulated `west boards` output into the list, or the
    /// failure the picker should explain instead.
    fn finish_boards_fetch(&mut self, outcome: &Outcome) {
        self.boards_process = None;
        self.boards = match outcome {
            Outcome::Success => BoardsState::Loaded(parse_boards(&self.boards_output)),
            Outcome::SpawnFailed(reason) => BoardsState::Failed(format!(
                "could not start the board list ({reason}) — is west on PATH?"
            )),
            _ => BoardsState::Failed(format!("west boards failed ({})", outcome.summary())),
        };
    }

    fn finish(
        &mut self,
        running: Running,
        outcome: &Outcome,
        duration: Duration,
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
        // what was just configured. A pick stays a pick when the cache
        // agrees with it (rebuilding on the picked board reconfigures the
        // cache to that name); only a cache the pick does not explain
        // demotes the answer back to "from build/".
        if ok && running.updates_board {
            let cached = cached_board(&self.root).map(|name| BoardChoice {
                name,
                origin: BoardOrigin::Cache,
            });
            self.board = match (&self.board, cached) {
                (Some(picked), Some(cached))
                    if picked.origin == BoardOrigin::Picked && picked.name == cached.name =>
                {
                    Some(picked.clone())
                }
                (_, cached) => cached,
            };
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

/// Reads the board a configured build directory targets, from
/// `build/zephyr/CMakeCache.txt`'s `CACHED_BOARD:STRING=` entry. `None` when
/// there is no build directory yet, or a cache without the entry (a cache
/// written by something other than a Zephyr app build).
pub fn cached_board(root: &Path) -> Option<String> {
    let cache = std::fs::read_to_string(root.join("build/zephyr/CMakeCache.txt")).ok()?;
    cache
        .lines()
        .find_map(|line| line.strip_prefix(CACHED_BOARD_KEY))
        .map(str::trim)
        .filter(|board| !board.is_empty())
        .map(str::to_string)
}

/// Parses `west boards` output: one target per line as `name description`
/// (description optional; HWMv2 names carry a `/`). Blank and non-matching
/// lines are skipped rather than fatal --- the list is hundreds of lines
/// long and one odd row must not empty it.
pub fn parse_boards(text: &str) -> Vec<Board> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let description = parts.collect::<Vec<_>>().join(" ");
            Some(Board {
                name: name.to_string(),
                description,
            })
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
        assert_eq!(cached_board(&dir).as_deref(), Some("nrf52840dk/nrf52840"));
    }

    #[test]
    fn missing_or_boardless_cache_means_no_board() {
        let dir = fixture_dir("nocache");
        assert_eq!(cached_board(&dir), None);

        std::fs::write(
            dir.join("build/zephyr/CMakeCache.txt"),
            "CACHED_BOARD:STRING=\n",
        )
        .unwrap();
        assert_eq!(cached_board(&dir), None, "an empty board is no board");
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
        panel.boards = BoardsState::Loaded(parse_boards(
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
    fn the_action_list_bookends_with_stop_flash_and_board() {
        let mut panel = BuildPanel::new("/nonexistent", UtcOffset::UTC);
        // Zephyr's real set: build lifecycle, flash, board.
        let zephyr = crate::backend::Capabilities::from_slice(&[
            crate::backend::Capability::Build,
            crate::backend::Capability::Clean,
            crate::backend::Capability::Flash,
            crate::backend::Capability::BoardSelect,
        ]);
        assert_eq!(
            panel.actions(&zephyr),
            vec![
                BuildAction::Build(BuildKind::Build),
                BuildAction::Build(BuildKind::Clean),
                BuildAction::Build(BuildKind::Rebuild),
                BuildAction::Flash,
                BuildAction::Board,
            ]
        );
        assert_eq!(panel.action_at(&zephyr, 3), Some(BuildAction::Flash));
        assert_eq!(panel.action_at(&zephyr, 4), Some(BuildAction::Board));

        // A backend without flash/board capability: just the lifecycle.
        let plain = crate::backend::Capabilities::from_slice(&[crate::backend::Capability::Build]);
        assert_eq!(panel.action_at(&plain, 3), None, "no more rows");

        // With a command running, Stop shifts everything down by one ---
        // the cursor arithmetic the key handler depends on.
        let mut processes = ProcessManager::new();
        panel.start(
            BuildKind::Build.label(),
            true,
            crate::process::Command::new(fake("west")),
            &mut processes,
        );
        assert_eq!(panel.action_at(&zephyr, 0), Some(BuildAction::Stop));
        assert_eq!(panel.action_at(&zephyr, 5), Some(BuildAction::Board));
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
}
