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

/// Lines of build output kept for the Monitor tab. A full Zephyr build
/// prints thousands; the tail is what diagnoses a failure.
const OUTPUT_CAPACITY: usize = 2_000;

/// The `CMakeCache.txt` entry holding the board a build directory was
/// configured for (`build/zephyr/CMakeCache.txt` on a normal application).
const CACHED_BOARD_KEY: &str = "CACHED_BOARD:STRING=";

/// Result of the last finished command, for the panel's header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildReport {
    pub kind: BuildKind,
    pub ok: bool,
    pub duration: Duration,
    /// Wall-clock finish time, in the app's configured local offset.
    pub at: OffsetDateTime,
}

struct Running {
    id: ProcessId,
    kind: BuildKind,
    started: Instant,
}

pub struct BuildPanel {
    /// Project root: the working directory every command runs in (`west`
    /// finds its workspace from the cwd).
    pub root: PathBuf,
    /// Board the build directory was configured for, from `CMakeCache.txt`.
    /// `None` until a build exists --- or forever, before the first one.
    pub board: Option<String>,
    pub cursor: usize,
    pub last: Option<BuildReport>,
    pub output: VecDeque<String>,
    offset: UtcOffset,
    running: Option<Running>,
    /// Overrides the executable the backend's commands name (`west`), the
    /// seam tests (and later `[tools]` config) plug a substitute into.
    tool_path: Option<String>,
}

impl BuildPanel {
    pub fn new(root: impl Into<PathBuf>, offset: UtcOffset) -> Self {
        let root = root.into();
        let board = cached_board(&root);
        Self {
            root,
            board,
            cursor: 0,
            last: None,
            output: VecDeque::new(),
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

    /// Rows the action list shows: `Stop` in front of the lifecycle entries
    /// exactly while a command is running, so cancelling is as discoverable
    /// as starting (`SPEC.md` §12's cancellation requirement, in UI form).
    pub fn action_count(&self) -> usize {
        BuildKind::ALL.len() + usize::from(self.is_busy())
    }

    /// The action at `index` in the drawn list, mirroring the layout
    /// [`Self::action_count`] describes: `None` is the `Stop` row (or an
    /// out-of-range cursor).
    pub fn action_at(&self, index: usize) -> Option<BuildKind> {
        let index = index.checked_sub(usize::from(self.is_busy()))?;
        BuildKind::ALL.get(index).copied()
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
        let command = backend.build_command(kind, self.board.as_deref(), self.has_build_dir())?;
        let command = command.current_dir(&self.root);
        Some(match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        })
    }

    fn has_build_dir(&self) -> bool {
        self.root.join("build").is_dir()
    }

    /// Starts `command` as this panel's running process. `false` (without
    /// side effects) when something is already running --- one build at a
    /// time, same rule as the flash panel.
    pub fn start(
        &mut self,
        kind: BuildKind,
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
            kind,
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
    pub fn on_process(&mut self, event: &ProcessEvent) -> Vec<(Level, String)> {
        match event {
            ProcessEvent::Line { id, text, .. } => {
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.id == *id)
                {
                    self.push_output(text.clone());
                }
                Vec::new()
            }
            ProcessEvent::Finished {
                id,
                outcome,
                duration,
            } => {
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

    fn finish(
        &mut self,
        running: Running,
        outcome: &Outcome,
        duration: Duration,
    ) -> Vec<(Level, String)> {
        let ok = outcome.is_success();
        let what = running.kind.label();
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
            kind: running.kind,
            ok,
            duration,
            at: OffsetDateTime::now_utc().to_offset(self.offset),
        });
        // A finished build/rebuild leaves a fresh CMakeCache behind: re-read
        // it so the header (and the next `--pristine=always -b …`) tracks
        // what was just configured.
        if ok && matches!(running.kind, BuildKind::Build | BuildKind::Rebuild) {
            self.board = cached_board(&self.root);
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
}
