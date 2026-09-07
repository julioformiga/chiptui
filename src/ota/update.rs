//! The OTA panel: prepare and update behind one modal, and the update
//! cycle as a runner over the driver's declared stages.
//!
//! The panel *contains* a [`Prepare`] (the M4 instrumentation flow ---
//! requirements probes included) and adds the update half on top: the
//! stages of [`OtaMethodDriver::stages`], walked in order, each spawned
//! through the [`ProcessManager`] with its own [`OtaStage::timeout`], the
//! post-`Reset` dead time honoured as a [`OtaStage::settle`] rather than a
//! longer timeout, and every stage's [`OtaMethodDriver::read_answer`]
//! applied before the next is built --- `MarkPending` names the hash
//! `ReadState` answered.
//!
//! The one rule that shapes the whole flow: **the runner halts in front of
//! `Confirm`.** A verified swap leaves the image unconfirmed, the button
//! becomes `ConfirmImage`, and the state line says the next reset reverts
//! --- the safety net the mechanism exists to provide is never thrown away
//! by the tool running on ahead.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::backend::zephyr::domains::Domains;
use crate::process::{Outcome, ProcessEvent, ProcessId, ProcessManager};
use crate::progress::Progress;
use crate::project::config;
use crate::stepper::{Phase, StepState};

use super::prepare::Prepare;
use super::{OtaConfig, OtaContext, OtaMethodDriver, OtaStage, registry};

/// Lines of stage output kept. Every stage together produces a few dozen ---
/// an upload is silent from start to finish (see
/// [`super::mcumgr::McumgrDriver::progress`]) --- so this is headroom, and
/// what it is headroom *for* is the tail that explains a failure.
const OUTPUT_CAPACITY: usize = 2_000;

/// What the panel's one action button *is*, right now.
///
/// The button's label, whether it is enabled, and what pressing it does are
/// one decision, made by [`OtaPanel::action`] and read by both the renderer
/// ([`crate::ui`]) and the key handler
/// ([`crate::app::App::on_ota_key`]) --- `install::Action`'s lesson, copied
/// verbatim: deciding separately is how a panel ends up drawing an
/// enabled-looking button no keypress can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtaAction {
    /// A stage is running (or the post-reset settle is): the button is
    /// `■ Stop`.
    Stop,
    /// A blocking requirement is unanswered (the `smpmgr` probe, the slot
    /// precondition). Drawn dim --- the checklist above is the reason.
    Blocked,
    /// Everything is there but the device's address. Pressing opens the
    /// address entry.
    SetAddress,
    /// The project is not instrumented; pressing asks, then prepares it.
    Prepare,
    /// Instrumented, but the build directory holds no signed image.
    /// Pressing starts the pristine sysbuild rebuild that writes one ---
    /// through the *build panel's* process slot, not this modal's, which is
    /// why the modal closes first (a command of minutes belongs in the
    /// Monitor with `Stop` reachable, the `ZephyrActions`/`Dashboard` rule).
    Rebuild,
    /// Everything is there; pressing asks, then runs the cycle.
    Update,
    /// The prepare stopped on a failed step; pressing re-asks its question.
    RetryPrepare,
    /// A cycle stage failed (or a settle was cut short); pressing re-asks
    /// the update question and resumes from that stage.
    RetryUpdate,
    /// The `Confirm` stage itself failed; pressing re-asks the confirm.
    ///
    /// This variant exists because one shared `Retry` labelled itself
    /// `"Update"` while `retry_confirm` routed it to the confirm question:
    /// the button's word contradicted its effect, which is the exact
    /// failure this type's doc comment above warns about.
    RetryConfirm,
    /// The swap landed and the image is unconfirmed. The most important
    /// row in the design: pressing is the user's *separate* decision to
    /// make the running image permanent.
    ConfirmImage,
    /// Update confirmed; nothing left to do. Dim.
    Done,
}

impl OtaAction {
    /// A retry is labelled by *what it retries*, never by the sequence it
    /// happens to live in: the three used to share one `"Update"`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Blocked | Self::Update | Self::RetryUpdate => "Update",
            Self::SetAddress => "Set the address",
            Self::Prepare | Self::RetryPrepare => "Prepare",
            // The words the state line above it already uses for the fix.
            Self::Rebuild => "Rebuild (pristine)",
            Self::ConfirmImage | Self::RetryConfirm => "Confirm image",
            Self::Done => "Done",
        }
    }

    /// The button's leading glyph --- colored in `ui::ota` (this type stays
    /// UI-free, the same split the installer's `Action` keeps).
    pub const fn icon(self, icons: crate::icons::IconSet) -> &'static str {
        match self {
            Self::Stop => icons.stop(),
            Self::Blocked
            | Self::SetAddress
            | Self::Prepare
            | Self::RetryPrepare
            | Self::Rebuild
            | Self::Update
            | Self::RetryUpdate => icons.play(),
            Self::ConfirmImage | Self::RetryConfirm | Self::Done => icons.check(),
        }
    }

    /// Which question this action asks before it acts, when it asks one.
    /// Living on the variant is what keeps the label and the confirm from
    /// drifting apart again --- they are read from the same value.
    pub const fn confirm(self) -> Option<OtaConfirm> {
        match self {
            Self::Prepare | Self::RetryPrepare => Some(OtaConfirm::Prepare),
            Self::Update | Self::RetryUpdate => Some(OtaConfirm::Update),
            Self::ConfirmImage | Self::RetryConfirm => Some(OtaConfirm::ConfirmImage),
            _ => None,
        }
    }

    /// Whether pressing it does anything.
    ///
    /// Only `Blocked` does not: its explanation is the checklist above it.
    /// `Done` used to be dim too, which made it a button whose word
    /// promised an action it refused to perform --- "Done" reads as "close
    /// this", and `Esc` was the only way out.
    pub const fn enabled(self) -> bool {
        !matches!(self, Self::Blocked)
    }
}

/// What the update cycle is asking the board to confirm, and how the
/// confirm dialog quotes it --- one variant per question, one dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtaConfirm {
    /// Write the instrumentation files into the project.
    Prepare,
    /// Push the signed image and swap to it.
    Update,
    /// Make the just-swapped image permanent.
    ConfirmImage,
}

struct Run {
    id: ProcessId,
    stage: usize,
    started: Instant,
    /// Whether finishing this stage advances the cycle. A standalone probe
    /// (`p`) is a question about the board, not the first step of an
    /// update the user has not agreed to --- without this it succeeded and
    /// went straight on to upload an image.
    chain: bool,
}

/// What a process event left for the app to act on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OtaUpdate {
    /// A line for the log, when something went wrong or worth recording.
    pub notice: Option<String>,
    /// The cycle reached `ConfirmImage`: uploaded, swapped, verified ---
    /// and parked unconfirmed by design.
    pub halted_unconfirmed: bool,
    /// The confirm stage finished: the update is permanent.
    pub finished: bool,
}

/// The OTA modal's state: one project's prepare + update.
pub struct OtaPanel {
    /// The instrumentation flow, with the requirement probes inside it.
    pub prepare: Prepare,
    driver: &'static dyn OtaMethodDriver,
    build_dir: Option<String>,
    /// The signed image the update pushes, resolved from the build's
    /// `domains.yaml` --- refreshed on the tick, never guessed by name.
    image: Option<PathBuf>,
    /// One state per [`OtaMethodDriver::stages`] entry, same order.
    pub stages: Vec<StepState>,
    /// The hash `ReadState` answered: `MarkPending` names it and `Verify`
    /// checks the swap against it.
    slot_hash: Option<String>,
    running: Option<Run>,
    /// The post-`Reset` dead time's end. The bootloader's swap is ~45 s of
    /// a board that answers nothing, and no command covers it.
    settling_until: Option<Instant>,
    /// `Verify` passed: parked in front of `Confirm` until the user says so.
    awaiting_confirm: bool,
    /// Where the update cycle stands, for the state line.
    pub update_phase: Phase,
    pub output: VecDeque<String>,
    /// Rows scrolled up from the tail; 0 follows the tail.
    pub output_scroll: usize,
    /// The last progress shape a stage's output carried.
    progress: Option<Progress>,
    /// The client program --- the test seam.
    tool: String,
    /// Overrides the post-`Reset` settle, the seam a test drives: a real
    /// swap is ~45 s of a board answering nothing, and no test should wait
    /// it out.
    settle_override: Option<Duration>,
}

impl OtaPanel {
    /// A panel for `root`. `None` only when the config names a mechanism
    /// with no driver registered --- which `parse_ota`'s never-default rule
    /// already makes unreachable, so the caller logs and keeps the modal
    /// closed rather than recovering.
    pub fn new(
        root: impl Into<PathBuf>,
        board: impl Into<String>,
        build_dir: Option<String>,
        config: OtaConfig,
    ) -> Option<Self> {
        let driver = registry::driver_for(config.method)?;
        let prepare = Prepare::new(root, board, build_dir.clone(), config);
        let image = resolve_image(&prepare.root, build_dir.as_deref());
        Some(Self {
            prepare,
            stages: driver.stages().iter().map(|_| StepState::Pending).collect(),
            driver,
            build_dir,
            image,
            slot_hash: None,
            running: None,
            settling_until: None,
            awaiting_confirm: false,
            update_phase: Phase::Idle,
            output: VecDeque::new(),
            output_scroll: 0,
            progress: None,
            tool: super::mcumgr::PROGRAM.to_string(),
            settle_override: None,
        })
    }

    /// Whether this panel already describes that project, board and build
    /// directory --- the question [`crate::app::App::open_ota`] asks before
    /// it builds a new one, so reopening the modal does not discard a
    /// cycle's state (an unconfirmed image above all).
    pub fn serves(&self, root: &Path, board: &str, build_dir: Option<&str>) -> bool {
        self.prepare.root == root
            && self.prepare.board() == board
            && self.build_dir.as_deref() == build_dir
    }

    /// Runs the driver's first stage alone, with no confirm: it is a read
    /// (`os echo`), and it is the only way to find out whether the
    /// hand-typed address reaches a board before committing to an upload.
    pub fn probe_board(&mut self, processes: &mut ProcessManager) -> bool {
        if self.is_busy() {
            return false;
        }
        let Some(index) = self
            .driver
            .stages()
            .iter()
            .position(|stage| *stage == OtaStage::Probe)
        else {
            return false;
        };
        // A probe is a question about the board, not a resumption of the
        // cycle: it leaves the phase alone so a halted or stopped run keeps
        // saying what it was saying.
        self.stages[index] = StepState::Pending;
        self.start_stage(index, processes, false)
    }

    /// Answers the transport question, persisted the way the address is.
    ///
    /// Nothing else has to move: the Kconfig body the prepare writes is
    /// keyed off the transport, so `Step::BoardConfig` reads as pending
    /// again on its own and the button returns to `Prepare`.
    pub fn set_transport(&mut self, transport: super::Transport) -> std::io::Result<()> {
        let mut config = self.config().clone();
        config.transport = transport;
        config::save_ota(&self.prepare.root.join(config::FILE_NAME), &config)?;
        self.prepare.set_config(config);
        Ok(())
    }

    /// Points the client at a specific program --- the seam that keeps
    /// tests off `PATH`. Applies to the stage commands and, through the
    /// embedded [`Prepare`], to the requirement probe.
    pub fn set_tool(&mut self, program: impl Into<String>) {
        let program = program.into();
        self.prepare.set_tool(program.clone());
        self.tool = program;
    }

    /// Ends a finished cycle: the stages go back to `Pending` and the
    /// phase to `Idle`, while the prepare --- which describes the *project*,
    /// not this run --- is left alone.
    ///
    /// Pressing `Done` has to do this, not merely close the window. The
    /// panel now survives the overlay closing (so an unconfirmed halt
    /// cannot be discarded by `Esc`), and `action()` answers `Done` for as
    /// long as the phase is `Finished` --- so a modal that only closed
    /// would reopen on `Done` for ever and put a second update out of reach
    /// for the rest of the session. A confirmed update is permanent; there
    /// is nothing about the cycle left to preserve.
    pub fn end_cycle(&mut self) {
        self.stages = self
            .driver
            .stages()
            .iter()
            .map(|_| StepState::Pending)
            .collect();
        self.update_phase = Phase::Idle;
        self.awaiting_confirm = false;
        self.slot_hash = None;
        self.progress = None;
        // The output stays: it is the transcript of what happened, and the
        // next cycle appends to it the way every other run does.
    }

    /// Puts the panel where a verified swap leaves it, for tests that need
    /// the halt without spending a whole cycle reaching it.
    pub fn mark_awaiting_confirm_for_test(&mut self) {
        self.awaiting_confirm = true;
    }

    /// Fails a stage the way the runner would, for tests about what the
    /// panel *shows* afterwards rather than about how it got there.
    pub fn fail_stage_for_test(&mut self, index: usize, reason: String) {
        let mut update = OtaUpdate::default();
        self.fail_stage(index, reason, &mut update);
    }

    /// Shortens the post-`Reset` settle --- the test seam: the real wait is
    /// the bootloader's, and a test cannot spend it.
    pub fn set_settle(&mut self, settle: Duration) {
        self.settle_override = Some(settle);
    }

    /// Starts (or re-probes) the requirement queries --- the panel's only
    /// processes until a stage runs.
    pub fn probe_requirements(&mut self, processes: &mut ProcessManager) {
        self.prepare.probe_requirements(processes);
    }

    /// The answers the update runs against.
    pub fn config(&self) -> &OtaConfig {
        self.prepare.config()
    }

    /// The board the project builds for (the Target row's other half).
    pub fn board(&self) -> &str {
        self.prepare.board()
    }

    /// The signed image, when the build has produced one.
    pub fn image(&self) -> Option<&Path> {
        self.image.as_deref()
    }

    /// The build directory the update's image is resolved from, for the
    /// confirm dialog's quoted build.
    pub fn build_dir(&self) -> Option<&str> {
        self.build_dir.as_deref()
    }

    /// The stages the driver runs, for the checklist.
    pub fn stage_list(&self) -> &'static [OtaStage] {
        self.driver.stages()
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some() || self.settling_until.is_some()
    }

    /// The running stage, for the state line.
    pub fn running_stage(&self) -> Option<OtaStage> {
        self.running
            .as_ref()
            .map(|run| self.driver.stages()[run.stage])
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.running.as_ref().map(|run| run.started.elapsed())
    }

    /// The settle's remaining seconds, while the bootloader swaps.
    pub fn settling_remaining(&self) -> Option<Duration> {
        self.settling_until
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// The last progress shape read, for the state line.
    pub fn progress(&self) -> Option<Progress> {
        self.progress
    }

    /// The hash the cycle armed, once `ReadState` answered.
    pub fn slot_hash(&self) -> Option<&str> {
        self.slot_hash.as_deref()
    }

    /// Whether the cycle is parked in front of `Confirm`.
    pub fn awaiting_confirm(&self) -> bool {
        self.awaiting_confirm
    }

    /// What the action button is right now --- see [`OtaAction`]. The
    /// order *is* the logic: what is running outranks what failed, what
    /// failed outranks what is missing, and prepare-vs-update is answered
    /// by the project, never asked of the user.
    pub fn action(&self) -> OtaAction {
        if self.is_busy() {
            return OtaAction::Stop;
        }
        if !self.prepare.requirements_ready() || self.prepare.slots.blocks() {
            return OtaAction::Blocked;
        }
        if self.prepare.stopped() {
            return OtaAction::RetryPrepare;
        }
        // A retry names the stage it resumes: the `Confirm` stage is its own
        // question, and labelling it "Update" said one thing while pressing
        // it did another.
        if let Some(index) = self.failed_stage() {
            return if self.driver.stages()[index] == OtaStage::Confirm {
                OtaAction::RetryConfirm
            } else {
                OtaAction::RetryUpdate
            };
        }
        // A settle the user cut short leaves the cycle mid-flight with no
        // failed stage: resuming is still the question, and `next_stage`
        // answers where.
        if matches!(self.update_phase, Phase::Stopped(_)) {
            return OtaAction::RetryUpdate;
        }
        if self.awaiting_confirm {
            return OtaAction::ConfirmImage;
        }
        if matches!(self.update_phase, Phase::Finished) {
            return OtaAction::Done;
        }
        if self.prepare.next_step().is_some() {
            return OtaAction::Prepare;
        }
        if self.image.is_none() {
            return OtaAction::Rebuild;
        }
        if self.config().address.is_none() {
            return OtaAction::SetAddress;
        }
        OtaAction::Update
    }

    fn failed_stage(&self) -> Option<usize> {
        self.stages
            .iter()
            .position(|state| matches!(state, StepState::Failed(_)))
    }

    /// The first stage still to run for the cycle --- never `Confirm`,
    /// which runs only from [`Self::confirm_image`].
    fn next_stage(&self) -> Option<usize> {
        self.driver
            .stages()
            .iter()
            .enumerate()
            .find(|(index, stage)| {
                **stage != OtaStage::Confirm
                    && matches!(
                        self.stages[*index],
                        StepState::Pending | StepState::Failed(_)
                    )
            })
            .map(|(index, _)| index)
    }

    /// The command a stage would run right now, for its row to quote --- or
    /// the driver's own refusal, in the command's place.
    ///
    /// The refusal is *returned*, not discarded. A `.ok()` here meant that a
    /// project with no address rendered all seven rows as "waiting on an
    /// earlier stage", which is untrue: they wait on the address, and the
    /// driver says so by name (`no IP address configured --- ...`). Showing
    /// the reason where the command would go is the rule `flash_plan`
    /// already follows.
    pub fn stage_command(&self, index: usize) -> Result<crate::process::Command, String> {
        let Some(stage) = self.driver.stages().get(index).copied() else {
            return Err("no such stage".to_string());
        };
        let image = self.image.clone().unwrap_or_else(|| {
            // A placeholder for the row preview only: an upload with no
            // image can never be *started* (`OtaAction::Rebuild`), so the
            // path shown here never reaches a process.
            PathBuf::from("<build first>")
        });
        let context = OtaContext {
            target: self.config(),
            image: &image,
            slot_hash: self.slot_hash.as_deref(),
            tool: &self.tool,
        };
        self.driver.stage_command(stage, &context)
    }

    /// The command the next `Update` would actually start --- what the
    /// confirm quotes, so a resumed cycle does not promise an upload it is
    /// past. `None` when nothing is left to resume.
    pub fn next_stage_command(
        &self,
    ) -> Option<(OtaStage, Result<crate::process::Command, String>)> {
        let index = self.next_stage()?;
        Some((self.driver.stages()[index], self.stage_command(index)))
    }

    /// Re-resolves the signed image --- a build that landed while the modal
    /// was open is how `BuildFirst` becomes `Update`. Tick-driven.
    pub fn refresh_image(&mut self) {
        self.image = resolve_image(&self.prepare.root, self.build_dir.as_deref());
    }

    /// Answers the address question, persisted through the same surgical
    /// writer the prepare's record step uses, and re-answered inside the
    /// prepare flow so its record step stays honest about what is on disk.
    pub fn set_address(&mut self, address: String) -> std::io::Result<()> {
        let mut config = self.config().clone();
        config.address = Some(address);
        config::save_ota(&self.prepare.root.join(config::FILE_NAME), &config)?;
        self.prepare.set_config(config);
        Ok(())
    }

    /// Starts the prepare half: the writes are synchronous, so this settles
    /// inside the call.
    pub fn start_prepare(&mut self) -> super::prepare::PrepareUpdate {
        let update = self.prepare.start();
        for line in self.prepare.output.clone() {
            self.push_output(line);
        }
        self.prepare.output.clear();
        update
    }

    /// Starts (or resumes) the update cycle at its first unfinished stage.
    pub fn start_update(&mut self, processes: &mut ProcessManager) -> bool {
        if self.is_busy() {
            return false;
        }
        let Some(index) = self.next_stage() else {
            return false;
        };
        self.update_phase = Phase::Running;
        self.start_stage(index, processes, true)
    }

    /// Runs the confirm stage alone --- the only way it runs.
    pub fn confirm_image(&mut self, processes: &mut ProcessManager) -> bool {
        if self.is_busy() {
            return false;
        }
        let Some(index) = self
            .driver
            .stages()
            .iter()
            .position(|stage| *stage == OtaStage::Confirm)
        else {
            return false;
        };
        self.update_phase = Phase::Running;
        self.start_stage(index, processes, true)
    }

    /// Cancels the running stage, or ends the settle early, at the user's
    /// request.
    pub fn stop(&mut self, processes: &mut ProcessManager) -> bool {
        if let Some(run) = &self.running {
            processes.cancel(run.id);
            return true;
        }
        if self.settling_until.take().is_some() {
            // The swap itself cannot be cancelled --- what stops is the
            // wait. The next `Update` resumes at `Verify`, which is the
            // honest next question either way.
            //
            // `Phase::Idle` here let `action()` fall through to `Update`,
            // whose state line claims the board answers and the image is
            // signed --- said of a board that is very likely still swapping
            // and unreachable. The stop is a stop: it says so, and the
            // button resumes.
            let reason =
                "the settle was ended early --- the board may still be swapping".to_string();
            self.push_output(format!("{reason}; 'Update' resumes at Verify"));
            self.update_phase = Phase::Stopped(reason);
            return true;
        }
        false
    }

    /// Drives the settle: the dead time after `Reset` ends on the tick, and
    /// the next stage starts. Nothing else here is tick-driven.
    pub fn tick(&mut self, processes: &mut ProcessManager) {
        self.refresh_image();
        let Some(deadline) = self.settling_until else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.settling_until = None;
        if let Some(index) = self.next_stage() {
            self.start_stage(index, processes, true);
        }
    }

    /// Feeds a process event back in: the embedded prepare's probes first
    /// (it guards on its own ids), then the running stage.
    pub fn on_process(
        &mut self,
        event: &ProcessEvent,
        processes: &mut ProcessManager,
    ) -> OtaUpdate {
        let mut update = OtaUpdate::default();
        self.prepare.on_process(event);
        match event {
            ProcessEvent::Line { id, text, .. } | ProcessEvent::Output { id, text } => {
                if self.is_stage(*id) {
                    if let Some(shape) = self.driver.progress(text) {
                        self.progress = Some(shape);
                    }
                    let text = text.clone();
                    self.push_output(text);
                }
            }
            ProcessEvent::Finished { id, outcome, .. } => {
                if self.is_stage(*id) {
                    self.finish_stage(outcome, processes, &mut update);
                }
            }
            ProcessEvent::Started { .. } | ProcessEvent::Bytes { .. } => {}
        }
        update
    }

    fn is_stage(&self, id: ProcessId) -> bool {
        self.running.as_ref().is_some_and(|run| run.id == id)
    }

    fn start_stage(&mut self, index: usize, processes: &mut ProcessManager, chain: bool) -> bool {
        let stage = self.driver.stages()[index];
        let image = match &self.image {
            Some(image) => image.clone(),
            None => PathBuf::from("<build first>"),
        };
        let context = OtaContext {
            target: self.config(),
            image: &image,
            slot_hash: self.slot_hash.as_deref(),
            tool: &self.tool,
        };
        let command = match self.driver.stage_command(stage, &context) {
            Ok(command) => command,
            Err(reason) => {
                self.stages[index] = StepState::Failed(reason.clone());
                self.update_phase = Phase::Stopped(reason);
                return false;
            }
        };
        // The literal command leads its own output: what streams below is
        // only meaningful attached to what produced it (`SPEC.md` §15's
        // never-hide-what-runs).
        self.push_output(format!("$ {command}"));
        self.progress = None;
        let id = processes.spawn(command, stage.timeout());
        self.stages[index] = StepState::Running;
        self.running = Some(Run {
            id,
            stage: index,
            started: Instant::now(),
            chain,
        });
        self.update_phase = Phase::Running;
        true
    }

    fn finish_stage(
        &mut self,
        outcome: &Outcome,
        processes: &mut ProcessManager,
        update: &mut OtaUpdate,
    ) {
        let Some(run) = self.running.take() else {
            return;
        };
        let index = run.stage;
        let stage = self.driver.stages()[index];
        match outcome {
            Outcome::Success => {
                if let Err(reason) = self.apply_answer(stage) {
                    self.fail_stage(index, reason, update);
                    return;
                }
                self.stages[index] = StepState::Done;
                // A one-shot: it answered its question and stops there.
                // Chaining made `p` upload an image nobody had agreed to
                // push.
                if !run.chain {
                    return;
                }
                if stage == OtaStage::Verify {
                    // The halt the whole design bends toward: the swap
                    // landed, and making it permanent is a separate,
                    // later, user decision.
                    self.awaiting_confirm = true;
                    update.halted_unconfirmed = true;
                    update.notice = Some(
                        "OTA: the new image is running, unconfirmed --- the next reset reverts it"
                            .to_string(),
                    );
                    return;
                }
                if stage == OtaStage::Confirm {
                    self.awaiting_confirm = false;
                    self.update_phase = Phase::Finished;
                    update.finished = true;
                    update.notice = Some("OTA: image confirmed".to_string());
                    return;
                }
                if stage.settle() > Duration::ZERO {
                    let settle = self.settle_override.unwrap_or_else(|| stage.settle());
                    self.settling_until = Some(Instant::now() + settle);
                    self.push_output(format!(
                        "the board is resetting; the bootloader's swap takes ~{}s",
                        stage.settle().as_secs()
                    ));
                    return;
                }
                match self.next_stage() {
                    Some(next) => {
                        self.start_stage(next, processes, true);
                    }
                    None => {
                        // Only reachable with a driver whose stages omit
                        // `Confirm` entirely: nothing to halt in front of.
                        self.update_phase = Phase::Finished;
                        update.finished = true;
                    }
                }
            }
            // A user stop is not a failure: the stage goes back to pending
            // and the cycle resumes from it on the next `Update`.
            Outcome::Cancelled => {
                self.stages[index] = StepState::Pending;
                self.update_phase = Phase::Idle;
                self.push_output(format!("{}: stopped by the user", stage.label()));
            }
            Outcome::Failed { .. } => {
                self.fail_stage(index, format!("failed ({})", outcome.summary()), update);
            }
            Outcome::TimedOut => {
                self.fail_stage(
                    index,
                    format!("timed out after {}s", stage.timeout().as_secs()),
                    update,
                );
            }
            Outcome::SpawnFailed(err) => {
                self.fail_stage(index, format!("could not start: {err}"), update);
            }
        }
    }

    /// Reads a finished stage's answer out of its output and applies it.
    fn apply_answer(&mut self, stage: OtaStage) -> Result<(), String> {
        let output = self.tail_text();
        match stage {
            OtaStage::ReadState => {
                let Some(hash) = self.driver.read_answer(stage, &output) else {
                    return Err(
                        "the state read answered no slot-1 hash --- is the image signed?"
                            .to_string(),
                    );
                };
                self.slot_hash = Some(hash);
                Ok(())
            }
            OtaStage::Verify => {
                let now = self.driver.read_answer(stage, &output);
                match (now, self.slot_hash.as_deref()) {
                    (Some(now), Some(armed)) if now == armed => Ok(()),
                    (Some(now), Some(armed)) => Err(format!(
                        "the swap did not take --- slot 0 runs {now} but the update armed {armed}"
                    )),
                    (Some(_), None) => Ok(()),
                    (None, _) => {
                        Err("the post-reset state read answered nothing to check".to_string())
                    }
                }
            }
            _ => Ok(()),
        }
    }

    /// The output of the stage that just finished: everything since its
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

    fn fail_stage(&mut self, index: usize, reason: String, update: &mut OtaUpdate) {
        let stage = self.driver.stages()[index];
        let reason = format!("{}: {reason}", stage.label());
        self.stages[index] = StepState::Failed(reason.clone());
        self.update_phase = Phase::Stopped(reason.clone());
        update.notice = Some(format!("OTA update: {reason}"));
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

/// The signed application image for the update: the default domain's
/// `zephyr.signed.bin`, resolved through the build's own `domains.yaml` ---
/// never a guess at the application's name. `None` for a build that is not
/// a sysbuild one (no `domains.yaml`) or has not produced the image yet:
/// both are the button's `BuildFirst` answer, not an error.
fn resolve_image(root: &Path, build_dir: Option<&str>) -> Option<PathBuf> {
    let build = root.join(build_dir?);
    let domains = Domains::read(&build)?;
    let image = domains
        .domain_dir(&domains.default)
        .join("zephyr")
        .join("zephyr.signed.bin");
    image.is_file().then_some(image)
}
