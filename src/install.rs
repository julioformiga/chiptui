//! The Zephyr installer: the getting-started guide, run and reported.
//!
//! ChipTUI can *validate* a Zephyr installation
//! ([`crate::backend::zephyr::workspace::install_check`]) but until now had
//! nothing to offer someone who has none. This panel is the answer: point it
//! at a folder and it runs the guide's sequence into `<folder>` --- pyenv,
//! venv, `west init`, `west update`, the SDK --- streaming every command's
//! output where the user can watch it.
//!
//! Two rules shape everything here:
//!
//! * **Nothing is installed on the user's behalf.** The system prerequisites
//!   (cmake, dtc, pyenv) are *reported* with the command that would install
//!   them ([`prereq`]); while a blocking one is unanswered the sequence
//!   cannot start. What the panel does run is confined to the folder it was
//!   given: a venv, a west workspace, an SDK bundle.
//! * **Nothing is overwritten.** Every step's result is detected from the
//!   filesystem ([`steps::Step::already_done`]), so an interrupted
//!   installation resumes where it stopped rather than starting over ---
//!   whether it was this app that stopped, or a reboot.
//!
//! The panel owns its **own** process slot rather than borrowing the build
//! panel's: its output belongs in its own overlay, not the Monitor tab, and
//! while it runs there is by definition no resolved workspace for a build
//! command to run in.

pub mod prereq;
pub mod steps;
pub mod version;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::process::{Command, Outcome, ProcessEvent, ProcessId, ProcessManager};

pub use prereq::{Prereq, PrereqState, Probe};
pub use steps::Step;
pub use version::Version;

/// `west update` clones every module the manifest names and `pyenv install`
/// compiles CPython; both are tens of minutes on a cold machine and a slow
/// link. The build panel's half hour is the closest precedent, doubled ---
/// a wedged step still cannot live forever.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// A version query answers in milliseconds or not at all.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Lines of installer output kept. `west update` prints one line per module
/// and `pip` several hundred; the tail is what explains a failure.
const OUTPUT_CAPACITY: usize = 2_000;

/// Where a step stands. `Skipped` is the user's answer to the SDK, not a
/// failure --- it is drawn differently and never blocks what follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Failed(String),
    Skipped,
}

/// What the panel as a whole is doing, for the state line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Probing prerequisites, or waiting for the user to start.
    Idle,
    Running,
    /// Every step reached `Done` or `Skipped`.
    Finished,
    /// A step failed; the sequence stopped there.
    Stopped(String),
}

/// What the panel's one action button *is*, right now.
///
/// The button's label, whether it is enabled, and what pressing it does are
/// one decision, made here and read by both the renderer
/// ([`crate::ui`]) and the key handler ([`crate::app::App::on_install_key`]).
/// They used to decide separately, which is exactly how the panel ended up
/// drawing an enabled-looking `▶ Install` that no keypress could act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A step is running: the button is `■ Stop`.
    Stop,
    /// A blocking prerequisite is unanswered. Drawn as a dim `▶ Install`
    /// --- the checklist above is the explanation.
    Blocked,
    /// The SDK will run but no toolchain is picked. Pressing opens the
    /// picker: with no `-t`, `west sdk install` pulls all 35 toolchains,
    /// so the question has to be answered --- but it is about the *last*
    /// step, so it must not hold up the eleven before it.
    PickToolchains,
    /// Run the sequence.
    Install,
    /// Resume from the step that failed.
    Retry,
    /// Record the installation already at the target; nothing runs.
    Adopt,
    /// Record it *and* run the one step it is missing.
    InstallSdk,
    /// The SDK is installed, but the user picked a toolchain it does not
    /// carry: run the SDK step for just that one. West skips the download
    /// when the version is already registered, so this costs one toolchain,
    /// not the whole bundle again.
    AddToolchains,
    /// Nothing left to do. Dim.
    Done,
}

impl Action {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Blocked | Self::Install => "Install",
            Self::PickToolchains => "Pick SDK toolchains",
            Self::Retry => "Retry",
            Self::Adopt => "Use this installation",
            Self::InstallSdk => "Install the SDK",
            Self::AddToolchains => "Add SDK toolchains",
            Self::Done => "Done",
        }
    }

    /// The button's leading glyph --- colored in `ui::install` (this type
    /// stays UI-free, the same split `ui::flash`/`ui::build` use for their
    /// own actions' colors). Which rendering the glyph comes from is the
    /// caller's [`IconSet`](crate::icons::IconSet).
    pub const fn icon(self, icons: crate::icons::IconSet) -> &'static str {
        match self {
            Self::Stop => icons.stop(),
            Self::Blocked
            | Self::Install
            | Self::PickToolchains
            | Self::Retry
            | Self::InstallSdk
            | Self::AddToolchains => icons.play(),
            Self::Adopt | Self::Done => icons.check(),
        }
    }

    /// Whether pressing it does anything. The two that do not are the two
    /// whose explanation is already on screen: an open prerequisite, and
    /// nothing left to run.
    pub const fn enabled(self) -> bool {
        !matches!(self, Self::Blocked | Self::Done)
    }
}

pub struct Installer {
    /// The workspace root being built: `<picked folder>/zephyr`, or the
    /// picked folder itself when it already carries a `.west/` to resume.
    pub root: PathBuf,
    pub prereqs: Vec<PrereqState>,
    /// One state per [`Step::ALL`] entry, same order.
    pub steps: Vec<StepState>,
    /// The running step's process and its index, when one runs.
    running: Option<Run>,
    pub output: VecDeque<String>,
    /// Rows scrolled up from the tail of the output; 0 follows the tail.
    pub output_scroll: usize,
    pub phase: Phase,
    /// The 3.12.x [`Step::ResolvePython`] settled on.
    python: Option<String>,
    /// Where pyenv keeps its interpreters, from [`Step::PyenvRoot`].
    pyenv_root: Option<PathBuf>,
    /// The toolchains the user picked from [`steps::TOOLCHAINS`]. Never
    /// empty by the time the SDK step runs: an empty `-t` makes
    /// `west sdk install` pass `-t all`, which is 35 toolchains and several
    /// GB with no prompt, so [`Self::can_start`] refuses instead.
    pub picked_toolchains: Vec<String>,
    /// Whether the user declined the SDK steps.
    pub sdk_skipped: bool,
    /// Whether the target already *is* a Zephyr installation, so the panel
    /// offers to adopt it instead of running the sequence over it.
    ///
    /// Decided once, when the panel opens ([`Self::mark_already_installed`]),
    /// and never re-derived: the verdict must not change under the user
    /// mid-run, and on a *resumed* installation it starts holding right
    /// after `west update` while later steps are still queued. The panel is
    /// told the answer rather than resolving it, the same way
    /// [`crate::build::BuildPanel::workspace_installed`] is.
    adopted: bool,
    /// Per-tool program overrides, the same seam
    /// [`crate::build::BuildPanel::set_tool_path`] gives `west`: tests point
    /// the prerequisite queries and `pyenv` at fixtures instead of `PATH`.
    tool_paths: Vec<(&'static str, String)>,
}

struct Run {
    id: ProcessId,
    step: usize,
    started: Instant,
}

impl Installer {
    /// A panel rooted at `root`, with every step's completion already read
    /// off the filesystem --- an existing folder opens showing what is left
    /// to do, not the whole guide again.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let steps = Step::ALL
            .iter()
            .map(|step| {
                if step.already_done(&root) {
                    StepState::Done
                } else {
                    StepState::Pending
                }
            })
            .collect();
        let mut panel = Self {
            root,
            prereqs: Prereq::ALL.iter().copied().map(PrereqState::new).collect(),
            steps,
            running: None,
            output: VecDeque::new(),
            output_scroll: 0,
            phase: Phase::Idle,
            python: None,
            pyenv_root: None,
            picked_toolchains: Vec::new(),
            sdk_skipped: false,
            adopted: false,
            tool_paths: Vec::new(),
        };
        // A resumed pin is a fact on disk, not something to re-derive: the
        // version to build the venv from is written in `.python-version`.
        panel.python = panel.pinned_python();
        panel
    }

    /// Records that the target is already a complete installation: the
    /// panel's action becomes adopting it rather than installing.
    ///
    /// [`Step::already_done`] cannot answer this. The three queries and the
    /// two idempotent steps deliberately never resume, so [`Self::next_step`]
    /// is never `None` on a finished tree --- the honest predicate is
    /// [`crate::backend::zephyr::workspace::install_state`], which is what
    /// the caller consults.
    pub fn mark_already_installed(&mut self) {
        self.adopted = true;
    }

    /// Whether the panel is offering to adopt rather than to install.
    pub fn adopted(&self) -> bool {
        self.adopted
    }

    /// Points `tool` at a specific program. Applies to the prerequisite
    /// queries and to `pyenv`; the venv's own binaries are absolute paths
    /// derived from [`Self::root`] and need no override.
    pub fn set_tool_path(&mut self, tool: &'static str, program: impl Into<String>) {
        let program = program.into();
        match self.tool_paths.iter_mut().find(|(name, _)| *name == tool) {
            Some(entry) => entry.1 = program,
            None => self.tool_paths.push((tool, program)),
        }
    }

    fn tool(&self, name: &'static str) -> &str {
        self.tool_paths
            .iter()
            .find(|(tool, _)| *tool == name)
            .map_or(name, |(_, program)| program.as_str())
    }

    /// Applies a tool override to a built command, matched on the program
    /// name it carries. A venv binary is an absolute path and matches
    /// nothing, which is the point --- it is derived from [`Self::root`]
    /// and needs no seam.
    fn resolve(&self, command: Command) -> Command {
        match self
            .tool_paths
            .iter()
            .find(|(tool, _)| *tool == command.program())
        {
            Some((_, program)) => command.with_program(program.clone()),
            None => command,
        }
    }

    /// The version pinned in the workspace's `.python-version`, when a
    /// previous run (or the user) wrote one.
    fn pinned_python(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.root.join(".python-version")).ok()?;
        let version = text.lines().next()?.trim();
        (!version.is_empty()).then(|| version.to_string())
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    /// The step that stopped the sequence, if one did --- `running` is
    /// already cleared by then, so the state list is what holds the answer.
    pub fn failed_step(&self) -> Option<Step> {
        self.steps
            .iter()
            .position(|state| matches!(state, StepState::Failed(_)))
            .map(|index| Step::ALL[index])
    }

    /// The step currently running, for the state line.
    pub fn running_step(&self) -> Option<Step> {
        self.running.as_ref().map(|run| Step::ALL[run.step])
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.running.as_ref().map(|run| run.started.elapsed())
    }

    /// Whether every blocking prerequisite is answered. The sequence's one
    /// gate: see the module docs.
    pub fn prereqs_ready(&self) -> bool {
        self.prereqs.iter().all(PrereqState::satisfied)
    }

    /// Whether the sequence can be started right now. Never in adopt mode:
    /// there the action records a config line and runs nothing, so it is
    /// [`Self::adopted`] that shapes the button --- and it deliberately
    /// does not wait on the prerequisites, which gate *building* Zephyr,
    /// not recording where an existing one lives.
    ///
    /// The SDK question is **not** part of this. It is about the last of
    /// twelve steps, and gating the whole sequence on it left the panel
    /// unable to start at all --- see [`Action::PickToolchains`].
    pub fn can_start(&self) -> bool {
        !self.adopted && !self.is_busy() && self.prereqs_ready() && self.next_step().is_some()
    }

    /// Whether the SDK question is answered: toolchains picked, or the SDK
    /// declined outright. Unanswered, `west sdk install` would fall through
    /// to its own `-t all` --- 35 toolchains, several GB, no prompt --- so
    /// the download is never what happens by omission.
    pub fn sdk_ready(&self) -> bool {
        self.sdk_skipped || !self.picked_toolchains.is_empty()
    }

    /// Whether an SDK bundle is still missing from the target --- the fact
    /// that turns [`Action::Adopt`] into [`Action::InstallSdk`], so a
    /// workspace whose SDK step was skipped or failed is not a dead end.
    pub fn sdk_missing(&self) -> bool {
        !self.sdk_skipped && self.installed_sdk().is_none()
    }

    /// What the action button is right now --- see [`Action`].
    pub fn action(&self) -> Action {
        if self.is_busy() {
            return Action::Stop;
        }
        // Adopting comes before the prerequisite gate on purpose: cmake and
        // dtc gate *building* Zephyr, and neither adopting an installation
        // nor `west sdk install` needs them. Checking them first is what
        // made an unbuildable machine unable to even record a workspace it
        // already had.
        if self.adopted {
            if self.sdk_missing() {
                return if self.sdk_ready() {
                    Action::InstallSdk
                } else {
                    Action::PickToolchains
                };
            }
            // The bundle is there; what is left is whether it carries what
            // the user asked for.
            return if self.pending_toolchains().is_empty() {
                Action::Adopt
            } else {
                Action::AddToolchains
            };
        }
        if !self.prereqs_ready() {
            return Action::Blocked;
        }
        // Asked only when the answer is about to matter, and only once:
        // picking, or skipping the SDK, both answer it.
        if self.sdk_missing() && !self.sdk_ready() {
            return Action::PickToolchains;
        }
        if self.stopped() {
            return Action::Retry;
        }
        if self.next_step().is_some() {
            Action::Install
        } else {
            Action::Done
        }
    }

    /// Whether a failed step stopped the sequence --- the button then reads
    /// "retry" rather than "install".
    pub fn stopped(&self) -> bool {
        matches!(self.phase, Phase::Stopped(_))
    }

    /// The first step still to run, skipping what the filesystem already
    /// shows done and what the user declined. `None` when nothing is left.
    ///
    /// A *failed* step counts as still to run: that is what `Retry` resumes
    /// from. The auto-advance uses [`Self::next_step_from`] instead, so a
    /// failure it stepped over is not immediately picked up again.
    pub fn next_step(&self) -> Option<usize> {
        self.next_step_from(0)
    }

    /// [`Self::next_step`] searching from `start`.
    pub fn next_step_from(&self, start: usize) -> Option<usize> {
        // `position` counts from the skip, not from the start of the list:
        // this has to be `find`, or every answer past step 0 is wrong.
        Step::ALL
            .iter()
            .enumerate()
            .skip(start)
            .find(|(index, step)| {
                if self.sdk_skipped && step.belongs_to_sdk() {
                    return false;
                }
                matches!(
                    self.steps[*index],
                    StepState::Pending | StepState::Failed(_)
                )
            })
            .map(|(index, _)| index)
    }

    /// The picked toolchains that are **not** installed yet --- what the
    /// SDK step actually has to do.
    ///
    /// With the bundle already unpacked, `west sdk install` finds the
    /// version registered, skips the download entirely, and runs
    /// `setup.sh -t NAME` for each name it was given. So asking only for
    /// the missing ones is how a single toolchain gets added to an SDK
    /// that is otherwise complete.
    pub fn pending_toolchains(&self) -> Vec<String> {
        let installed = self
            .installed_sdk()
            .map(|sdk| steps::installed_toolchains(&sdk))
            .unwrap_or_default();
        self.picked_toolchains
            .iter()
            .filter(|name| !installed.contains(name))
            .cloned()
            .collect()
    }

    /// The toolchains already unpacked in this workspace's SDK, for the
    /// picker to mark.
    pub fn installed_toolchains(&self) -> Vec<String> {
        self.installed_sdk()
            .map(|sdk| steps::installed_toolchains(&sdk))
            .unwrap_or_default()
    }

    fn context(&self) -> steps::Context<'_> {
        steps::Context {
            root: &self.root,
            pyenv: self.tool("pyenv"),
            python: self.python.as_deref(),
            pyenv_root: self.pyenv_root.as_deref(),
            toolchains: self.pending_toolchains(),
        }
    }

    /// The command a step would run right now, for the overlay to quote
    /// under its label. `None` while an earlier query has not answered.
    pub fn step_command(&self, index: usize) -> Option<Command> {
        Step::ALL.get(index)?.command(&self.context())
    }

    /// Re-derives the SDK step's state after a pick changed. An installed
    /// bundle makes the step `Done`, but picking a toolchain it does not
    /// carry makes it something to run again --- and the checklist has to
    /// say so, since the button beside it does.
    pub fn refresh_sdk_step(&mut self) {
        let Some(index) = Step::ALL.iter().position(|step| *step == Step::SdkInstall) else {
            return;
        };
        if self.sdk_skipped || matches!(self.steps[index], StepState::Failed(_)) {
            return;
        }
        self.steps[index] =
            if Step::SdkInstall.already_done(&self.root) && self.pending_toolchains().is_empty() {
                StepState::Done
            } else {
                StepState::Pending
            };
    }

    /// Marks the SDK steps skipped (or un-skips them). A no-op once the SDK
    /// is installed --- there is nothing left to decline.
    pub fn toggle_sdk(&mut self) {
        if self.is_busy() {
            return;
        }
        self.sdk_skipped = !self.sdk_skipped;
        for (index, step) in Step::ALL.iter().enumerate() {
            if !step.belongs_to_sdk() || self.steps[index] == StepState::Done {
                continue;
            }
            self.steps[index] = if self.sdk_skipped {
                StepState::Skipped
            } else {
                StepState::Pending
            };
        }
    }

    /// Starts (or re-probes) every prerequisite query. Each is its own
    /// short process, all in flight at once --- they contend for nothing.
    pub fn probe_prereqs(&mut self, processes: &mut ProcessManager) {
        for state in &mut self.prereqs {
            if let Some(id) = state.process.take() {
                processes.cancel(id);
            }
            state.probe = Probe::Probing;
            state.output.clear();
            let command = state.prereq.query();
            let program = self
                .tool_paths
                .iter()
                .find(|(tool, _)| *tool == state.prereq.program())
                .map(|(_, program)| program.clone());
            let command = match program {
                Some(program) => command.with_program(program),
                None => command,
            };
            state.process = Some(processes.spawn(command, PROBE_TIMEOUT));
        }
    }

    /// Starts the next pending step, if one can start. `false` when the
    /// gate refuses, nothing is left, or the step's command is not
    /// constructible yet.
    pub fn start(&mut self, processes: &mut ProcessManager) -> bool {
        if !self.can_start() {
            return false;
        }
        let Some(index) = self.next_step() else {
            return false;
        };
        self.start_step(index, processes)
    }

    /// Runs the SDK step alone, for a workspace that is already installed
    /// and only missing its toolchain bundle.
    ///
    /// Deliberately not routed through [`Self::start`]: on an installed
    /// tree [`Self::next_step`] answers `0`, because the queries and the
    /// two idempotent steps never resume --- starting there would re-run
    /// the whole sequence over a finished workspace. [`Step::SdkInstall`]
    /// needs nothing from the steps before it (only the root and the
    /// picked toolchains), and the auto-advance chains on into the
    /// confirmation from there.
    pub fn start_sdk_only(&mut self, processes: &mut ProcessManager) -> bool {
        if self.is_busy() || !self.sdk_ready() {
            return false;
        }
        let Some(index) = Step::ALL.iter().position(|step| *step == Step::SdkInstall) else {
            return false;
        };
        self.start_step(index, processes)
    }

    fn start_step(&mut self, index: usize, processes: &mut ProcessManager) -> bool {
        let Some(command) = self.step_command(index) else {
            let step = Step::ALL[index];
            self.fail(
                index,
                format!(
                    "{} needs an answer an earlier step did not give",
                    step.label()
                ),
            );
            return false;
        };
        let command = self.resolve(command);
        // The literal command leads its own output: what streams below is
        // only meaningful attached to what produced it (`SPEC.md` §15's
        // never-hide-what-runs).
        self.push_output(format!("$ {command}"));
        let id = processes.spawn(command, STEP_TIMEOUT);
        self.steps[index] = StepState::Running;
        self.running = Some(Run {
            id,
            step: index,
            started: Instant::now(),
        });
        self.phase = Phase::Running;
        true
    }

    /// Cancels the running step at the user's request.
    pub fn stop(&mut self, processes: &mut ProcessManager) -> bool {
        let Some(run) = &self.running else {
            return false;
        };
        processes.cancel(run.id);
        true
    }

    /// Feeds a process event back in. Covers the running step and the
    /// prerequisite queries, each matched by id and each ignoring the
    /// other's events --- the same rule every panel here follows.
    ///
    /// Returns what the app has to act on: a finished sequence is what
    /// persists the workspace and moves the user on.
    pub fn on_process(
        &mut self,
        event: &ProcessEvent,
        processes: &mut ProcessManager,
    ) -> InstallUpdate {
        let mut update = InstallUpdate::default();
        match event {
            ProcessEvent::Line { id, text, .. } | ProcessEvent::Output { id, text } => {
                if self.is_step(*id) {
                    let text = text.clone();
                    self.push_output(text);
                } else if let Some(state) = self
                    .prereqs
                    .iter_mut()
                    .find(|state| state.process == Some(*id))
                {
                    state.output.push_str(text);
                    state.output.push('\n');
                }
            }
            ProcessEvent::Finished { id, outcome, .. } => {
                if self.is_step(*id) {
                    self.finish_step(*id, outcome, processes, &mut update);
                } else {
                    self.finish_probe(*id, outcome);
                }
            }
            // Raw PTY bytes belong to the Terminal tab's emulator alone;
            // the installer's steps are piped.
            ProcessEvent::Started { .. } | ProcessEvent::Bytes { .. } => {}
        }
        update
    }

    fn is_step(&self, id: ProcessId) -> bool {
        self.running.as_ref().is_some_and(|run| run.id == id)
    }

    fn finish_probe(&mut self, id: ProcessId, outcome: &Outcome) {
        let Some(state) = self
            .prereqs
            .iter_mut()
            .find(|state| state.process == Some(id))
        else {
            return;
        };
        state.process = None;
        state.probe = match outcome {
            // A tool that cannot be started at all is missing, whatever the
            // reason: `SpawnFailed` is the only outcome that distinguishes
            // "no such program" from "ran and said something".
            Outcome::SpawnFailed(_) => Probe::Missing,
            Outcome::Success | Outcome::Failed { .. } => {
                // Some tools report their version with a non-zero status;
                // the output is the answer, not the exit code.
                Probe::classify(state.prereq, version::parse(&state.output))
            }
            Outcome::TimedOut | Outcome::Cancelled => Probe::Missing,
        };
    }

    fn finish_step(
        &mut self,
        id: ProcessId,
        outcome: &Outcome,
        processes: &mut ProcessManager,
        update: &mut InstallUpdate,
    ) {
        let Some(run) = self.running.take() else {
            return;
        };
        debug_assert_eq!(run.id, id);
        let index = run.step;
        let step = Step::ALL[index];
        if !matches!(outcome, Outcome::Success) {
            let reason = format!("{} failed ({})", step.label(), outcome.summary());
            // A step whose result nothing downstream needs is recorded and
            // stepped over: the SDK confirmation failing does not undo the
            // install it was confirming.
            if step.optional() {
                self.steps[index] = StepState::Failed(reason.clone());
                update.notice = Some(format!("Zephyr installer: {reason}"));
            } else {
                self.fail(index, reason);
                update.notice = Some(format!("Zephyr installer: {}", self.stop_reason()));
                update.stopped = true;
                return;
            }
        } else {
            self.apply_answer(step);
            self.steps[index] = StepState::Done;
        }
        // From *past* this step, never from the top: an optional step that
        // just failed is still `Failed`, and `next_step` reports those so
        // `Retry` can resume from them --- searching from 0 here would pick
        // the same failure again, forever.
        match self.next_step_from(index + 1) {
            Some(next) => {
                self.start_step(next, processes);
            }
            None => {
                self.phase = Phase::Finished;
                update.finished = true;
            }
        }
    }

    /// Reads a query step's answer out of the output it just produced.
    fn apply_answer(&mut self, step: Step) {
        match step {
            Step::ResolvePython => {
                self.python = version::latest_release(&self.tail_text(), prereq::PYTHON_SERIES)
                    .map(|version| version.to_string())
                    .or_else(|| self.python.clone());
            }
            Step::PyenvRoot => {
                self.pyenv_root = prereq::pyenv_root(&self.tail_text());
            }
            _ => {}
        }
    }

    /// The output of the step that just finished: everything since its
    /// `$ command` header line.
    fn tail_text(&self) -> String {
        let start = self
            .output
            .iter()
            .rposition(|line| line.starts_with("$ "))
            .map_or(0, |index| index + 1);
        self.output
            .iter()
            .skip(start)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fail(&mut self, index: usize, reason: String) {
        self.steps[index] = StepState::Failed(reason.clone());
        self.phase = Phase::Stopped(reason);
        self.running = None;
    }

    pub fn stop_reason(&self) -> String {
        match &self.phase {
            Phase::Stopped(reason) => reason.clone(),
            _ => String::new(),
        }
    }

    /// The SDK bundle this installation left in the workspace, if any ---
    /// the answer that becomes `[zephyr] sdk`.
    pub fn installed_sdk(&self) -> Option<PathBuf> {
        steps::installed_sdk(&self.root)
    }

    fn push_output(&mut self, line: String) {
        if self.output.len() >= OUTPUT_CAPACITY {
            self.output.pop_front();
            // Scrolling counts rows from the tail, so dropping the head
            // would otherwise slide the view without the user moving.
            self.output_scroll = self.output_scroll.saturating_sub(1);
        }
        self.output.push_back(line);
    }

    /// Scrolls the output view. Positive `delta` moves up (into history),
    /// clamped to the buffer; 0 rows above means following the tail.
    pub fn scroll_output(&mut self, delta: isize, viewport: usize) {
        let max = self.output.len().saturating_sub(viewport);
        let next = self.output_scroll as isize + delta;
        self.output_scroll = next.clamp(0, max as isize) as usize;
    }
}

/// What a process event left for the app to act on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InstallUpdate {
    /// Every step reached `Done` or `Skipped`: the workspace is installed
    /// and can be persisted.
    pub finished: bool,
    /// A line for the log, when something went wrong.
    pub notice: Option<String>,
    /// A step failed and stopped the sequence. What is already on disk may
    /// still be a usable workspace, which is the app's cue to record it
    /// rather than lose the work that did succeed.
    pub stopped: bool,
}
