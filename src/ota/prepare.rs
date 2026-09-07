//! Preparing a Zephyr project for OTA: the instrumentation writes.
//!
//! The flow is modelled on [`crate::install`] --- same
//! [`StepState`]/[`Phase`], same requirements-then-steps split, same
//! `already_done` read off the filesystem so an interrupted (or repeated)
//! run resumes instead of restarting. The real divergence, stated: where
//! the installer's steps are subprocesses, every step here is a synchronous
//! filesystem write, so `start_step` performs the write and settles
//! immediately rather than spawning anything --- the only processes the
//! panel runs are the requirement *probes*, which is the one
//! [`ProcessManager`] slot it needs.
//!
//! What the steps write, and the rules they write under:
//!
//! * `sysbuild.conf` and the `boards/<target>.conf` Kconfig fragment are
//!   extended through [`scaffold`]'s guarded block --- a project may already
//!   have both, and only the managed block is ever touched.
//! * `VERSION` is written only when absent; one that exists but does not
//!   parse is a named failure, never an overwrite.
//! * `[ota]` in `chiptui.toml` goes through [`config::save_ota`], the one
//!   surgical writer.
//!
//! Nothing here decides *whether* a project should be prepared: the board
//! answer and the build directory arrive with the [`Prepare`] and the slot
//! precondition is reported, never worked around (see [`SlotCheck`]).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::backend::zephyr::report::{self, partitions::FlashLayout};
use crate::backend::zephyr::variants;
use crate::process::{Command, Outcome, ProcessEvent, ProcessId, ProcessManager};
use crate::project::{config, scaffold};
use crate::stepper::{Phase, StepState};

use super::{OtaConfig, Transport};

/// A version query answers in milliseconds or not at all.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Lines of write history kept. Five steps produce a handful; the buffer
/// outlives them only so the panel can show what happened.
const OUTPUT_CAPACITY: usize = 500;

/// The `sysbuild.conf` managed block. Picked up automatically by
/// `west build --sysbuild`: MCUboot boots slot0, and after a new image
/// lands in slot1 it swaps and boots that, reverting on the next reset
/// unless the image is confirmed. `SWAP_USING_MOVE` rather than
/// `SWAP_USING_SCRATCH`: the layouts this targets already give slot0/slot1
/// equal sizes, which is all the move algorithm needs, and no scratch
/// partition has to be carved out.
const SYSBUILD_BODY: &str = "\
# OTA needs two image slots and something to swap them.
SB_CONFIG_BOOTLOADER_MCUBOOT=y
SB_CONFIG_MCUBOOT_MODE_SWAP_USING_MOVE=y
";

/// The `VERSION` a project gets when it has none. Zephyr's idiom: feeds
/// imgtool's signing and shows up in `image state-read`, which is how an
/// update is told apart from what runs. `0.1.0` because the project existed
/// before its first OTA image did.
const VERSION_BODY: &str = "\
VERSION_MAJOR = 0
VERSION_MINOR = 1
PATCHLEVEL = 0
VERSION_TWEAK = 0
EXTRAVERSION =
";

/// The net shell, as its own guarded block (`ota-netshell`) rather than a
/// line in the OTA one: it costs ~96 KB of flash and ~3 KB of RAM, and it
/// is the device's only way to *tell* you its address (`net ipv4`), so the
/// enable is the user's call --- the panel's toggle, default off.
const NETSHELL_BODY: &str = "\
# 'net' shell root command --- 'net ipv4' is how the device tells you the
# address to push an update to. Costs ~96 KB flash and ~3 KB RAM.
CONFIG_NET_SHELL=y
";

/// The net shell's price, shown on its row so the toggle is an informed
/// decision.
pub const NETSHELL_COST: &str = "+96 KB flash, +3 KB RAM --- gives 'net ipv4' on the shell";

/// The Kconfig fragment body for the OTA block: the core every transport
/// needs, then the transport's own symbols, then --- for UDP --- the
/// situational tweaks as *commented* suggestions with their reasons,
/// because a wrong value there is worse than absence.
///
/// Every symbol was verified against Zephyr's own Kconfig tree; the
/// commented ones are the ones a project must decide for itself.
pub fn kconfig_body(transport: Transport) -> String {
    let mut body = "\
# MCUboot A/B updates driven by mcumgr. No application code: img_mgmt
# writes the uploaded image straight into slot1 and the bootloader performs
# the swap, reverting unless the image is confirmed.
CONFIG_BOOTLOADER_MCUBOOT=y
CONFIG_MCUMGR=y
# img: upload/state/test/confirm. os: reset (hence REBOOT).
CONFIG_MCUMGR_GRP_IMG=y
CONFIG_MCUMGR_GRP_OS=y
# Lets the client query the server's buffer sizes; without it smpmgr warns
# on every command.
CONFIG_MCUMGR_GRP_OS_MCUMGR_PARAMS=y
CONFIG_REBOOT=y
CONFIG_IMG_MANAGER=y
# Erase slot1 sector by sector as the upload advances; erasing a whole slot
# up front stalls the application for seconds.
CONFIG_IMG_ERASE_PROGRESSIVELY=y
CONFIG_STREAM_FLASH=y
CONFIG_ZCBOR=y
CONFIG_NET_BUF=y
CONFIG_CRC=y
"
    .to_string();
    match transport {
        Transport::Udp => {
            body.push_str(
                "\
# This symbol is a `depends on`, not a `select`: without NET_UDP and
# NET_SOCKETS --- and the network stack underneath them --- Kconfig drops it
# to n, the build still succeeds, and the board simply never answers. Which
# stack (Wi-Fi, Ethernet), and DHCP or static, is this application's
# architecture and not ChipTUI's to choose, so bring your own; the OTA
# panel's 'transport' row reads the built .config back and says whether it
# survived.
CONFIG_MCUMGR_TRANSPORT_UDP=y
CONFIG_MCUMGR_TRANSPORT_UDP_IPV4=y

# --- Situational: read the reasons before enabling ------------------------
# The default of 4 is not enough once OTA shares the stack with anything
# else: running out shows up as connections failing *and* as NTP silently
# failing --- two symptoms that look unrelated and are not.
# CONFIG_NET_MAX_CONN=8
# Nothing here speaks IPv6, and it costs RAM; with it, some resolvers pick
# AF_INET6 and then fail a plain IPv4 literal with EAI_ADDRFAMILY.
# CONFIG_NET_IPV6=n
",
            );
        }
        Transport::Serial => {
            body.push_str(
                "\
# The symbol really is UART: smpmgr's `--port` names the transport, Kconfig
# names the peripheral class. Its dependencies come with it --- they are the
# driver and the framing this transport is made of, not an architecture
# choice, the same reason the BLE tier brings BT_PERIPHERAL.
CONFIG_MCUMGR_TRANSPORT_UART=y
CONFIG_UART_MCUMGR=y
CONFIG_BASE64=y
CONFIG_CONSOLE=y
",
            );
        }
        Transport::Ble => {
            body.push_str(
                "\
# MCUMGR_TRANSPORT_BT depends on BT_PERIPHERAL, so the stack comes with it.
CONFIG_BT=y
CONFIG_BT_PERIPHERAL=y
CONFIG_MCUMGR_TRANSPORT_BT=y
",
            );
        }
    }
    body
}

/// Whether `text` enables `symbol` --- a `SYMBOL=y` line of its own, never
/// a comment mentioning one. Kconfig's own tolerant spacing (`SYM = y`)
/// counts; `=n`, a commented-out line, or a different symbol do not.
fn has_symbol(text: &str, symbol: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return false;
        }
        line.split_once('=')
            .is_some_and(|(key, value)| key.trim() == symbol && value.trim() == "y")
    })
}

/// Whether `text` is a VERSION file Zephyr's build accepts.
///
/// The predicate is `cmake/modules/version.cmake`'s own contract: the four
/// numeric fields (`VERSION_MAJOR`, `VERSION_MINOR`, `PATCHLEVEL`,
/// `VERSION_TWEAK`) each present as `NAME = <digits>` and at most 255, or
/// the configure step dies with `FATAL_ERROR`. `EXTRAVERSION` is optional
/// there, so it is here.
fn version_parses(text: &str) -> bool {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == name && !value.trim().is_empty())
                .then(|| value.trim().parse::<u32>().ok())
                .flatten()
        })
    };
    [
        "VERSION_MAJOR",
        "VERSION_MINOR",
        "PATCHLEVEL",
        "VERSION_TWEAK",
    ]
    .into_iter()
    .all(|name| field(name).is_some_and(|value| value <= 255))
}

/// Whether the board's flash layout can carry A/B updates, answered from a
/// build the project already has --- [`FlashLayout`]'s module docs explain
/// why an ordinary build is enough to ask. Reported, never worked around:
/// parsing the board's `.dts` *sources* instead would mean reimplementing
/// the devicetree preprocessor, and it would still be wrong for partitions
/// arriving via a shield.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotCheck {
    /// No configured build directory, or none of its `zephyr.dts` ---
    /// nothing was checked, and that must not block: refusing to prepare a
    /// project that was never built would be ChipTUI asserting no slots
    /// exist, which it cannot know.
    NotChecked,
    /// `<build>/zephyr/zephyr.dts` carries both slots (the path read).
    Found(PathBuf),
    /// The build resolved and the named nodes are absent. Blocks: a board
    /// without two slots cannot do A/B updates, and the checklist row is
    /// the explanation.
    Missing(Vec<&'static str>, PathBuf),
}

impl SlotCheck {
    /// Reads the answer out of the build's devicetree. Where that is
    /// depends on the build's own shape: a classic build's at
    /// `<build>/zephyr/zephyr.dts`; once the project is prepared, the
    /// sysbuild build's application domain carries it at
    /// `<build>/<default domain>/zephyr/zephyr.dts` (`domains.yaml` names
    /// it --- the app's flash map is the honest thing to read, and never a
    /// guessed name).
    pub fn of(root: &Path, build_dir: Option<&str>) -> Self {
        let Some(build_dir) = build_dir else {
            return Self::NotChecked;
        };
        let build = root.join(build_dir);
        let mut path = build.join("zephyr").join("zephyr.dts");
        if !path.exists()
            && let Some(domains) = crate::backend::zephyr::domains::Domains::read(&build)
        {
            path = domains
                .domain_dir(&domains.default)
                .join("zephyr")
                .join("zephyr.dts");
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::NotChecked;
        };
        let missing = FlashLayout::read(&report::devicetree::parse(&text)).missing_for_ab();
        if missing.is_empty() {
            Self::Found(path)
        } else {
            Self::Missing(missing, path)
        }
    }

    /// Whether this answer stops the prepare. Only a known-absent layout
    /// does.
    pub fn blocks(&self) -> bool {
        matches!(self, Self::Missing(..))
    }
}

/// Whether the build actually carries the transport the project asked for.
///
/// Every `MCUMGR_TRANSPORT_*` symbol is a `depends on`, never a `select`
/// (see [`Transport::requires`]), so a project whose configuration does not
/// meet them gets the symbol quietly dropped to `n`. Nothing fails: the
/// build succeeds, the prepare reports every step `Done`, `smpmgr` is
/// installed and correct --- and the board never answers, which reads like
/// a wrong address or dead hardware. This is the one failure in the flow
/// with no symptom of its own, so it gets a row.
///
/// Read from `.config`, the build's own record of what it settled on ---
/// the [`SlotCheck`] rule applied to the other half of the question. Never
/// worked around: which network stack a project runs is its architecture,
/// not ChipTUI's to choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportCheck {
    /// No build directory, or no `.config` in it. Nothing was checked, and
    /// like [`SlotCheck::NotChecked`] that must not block.
    NotChecked,
    /// The build settled with the symbol on.
    Enabled(PathBuf),
    /// The build could not have known: it ran before the fragment was
    /// written, or the fragment does not exist yet. `dashboard.py`'s own
    /// staleness test (`report_mtime < source_mtime`), which is what tells
    /// "not rebuilt yet" apart from a genuinely unmet dependency --- the
    /// distinction that decides whether the row is a warning or noise.
    Stale(PathBuf),
    /// The build is current and the symbol is *not* on: the dependencies
    /// were not met and Kconfig dropped it.
    Dropped(PathBuf),
}

impl TransportCheck {
    /// Reads the answer out of the build's `.config`, at whichever of the
    /// two shapes the build has --- the same classic/sysbuild-domain walk
    /// [`SlotCheck::of`] does, and for the same reason: the application
    /// domain is the one whose configuration carries the transport.
    pub fn of(root: &Path, build_dir: Option<&str>, transport: Transport, fragment: &Path) -> Self {
        let Some(build_dir) = build_dir else {
            return Self::NotChecked;
        };
        let build = root.join(build_dir);
        // A sysbuild build's *application domain* is the authority, and
        // unlike the devicetree this cannot be decided by asking whether
        // the top-level file exists: a sysbuild top level has no
        // `zephyr.dts` (which is why [`SlotCheck::of`] gets away with that
        // test) but it does have a `.config` of its own --- sysbuild's own
        // configuration, which carries none of the application's symbols.
        // Reading it reported `Dropped` for the reference project, whose
        // app domain has `CONFIG_MCUMGR_TRANSPORT_UDP=y` and works.
        let path = match crate::backend::zephyr::domains::Domains::read(&build) {
            Some(domains) => domains
                .domain_dir(&domains.default)
                .join("zephyr")
                .join(".config"),
            None => build.join("zephyr").join(".config"),
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::NotChecked;
        };
        if has_symbol(&text, transport.symbol()) {
            return Self::Enabled(path);
        }
        // Absent only means "dropped" if this build could have known the
        // fragment asks for it. A fragment that does not exist yet, or one
        // written after the build ran, means it could not --- and reporting
        // a dropped symbol there would accuse the user of an unmet
        // dependency for the ordinary case of not having rebuilt.
        let fragment = root.join(fragment);
        if !fragment.exists() || is_stale(&path, &fragment) {
            return Self::Stale(path);
        }
        Self::Dropped(path)
    }

    /// Never blocks. Unlike a missing slot layout --- which is a fact about
    /// the board --- this one has an innocent explanation the check cannot
    /// always rule out (a build shape `.config` was not found in, a domain
    /// layout that moved), and `p` proves the answer in a second either
    /// way. It reports; the user decides.
    pub fn blocks(&self) -> bool {
        false
    }
}

/// Whether `built` is older than `source` --- `dashboard.py`'s staleness
/// test, reused. An unreadable timestamp on either side is not staleness:
/// the answer has to be a fact, and "cannot tell" is not one.
fn is_stale(built: &Path, source: &Path) -> bool {
    let modified = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    };
    match (modified(built), modified(source)) {
        (Some(built), Some(source)) => built < source,
        _ => false,
    }
}

/// What the prepare needs that it cannot write for itself.
///
/// `smpmgr` is reported and **never installed** (`install::prereq`'s rule):
/// a missing one blocks the sequence, because preparing a project whose
/// updates nothing on this machine can push would be a checklist no one
/// could finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    Smpmgr,
}

impl Requirement {
    pub const ALL: &'static [Requirement] = &[Requirement::Smpmgr];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Smpmgr => "smpmgr",
        }
    }

    pub const fn program(self) -> &'static str {
        match self {
            Self::Smpmgr => super::mcumgr::PROGRAM,
        }
    }

    /// The version query --- smpmgr prints its version and exits.
    pub fn query(self, program: &str) -> Command {
        Command::new(program.to_string()).arg("--version")
    }

    /// Where to get it when missing, shown instead of installing anything.
    pub const fn install_hint(self) -> &'static str {
        match self {
            Self::Smpmgr => "pipx install smpmgr",
        }
    }
}

/// What a requirement's query found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolProbe {
    /// No answer yet --- an unanswered question is not a yes.
    Probing,
    /// Ran and answered (the first line it printed).
    Present(String),
    /// Could not be started, or never answered.
    Missing,
}

/// One requirement's probe state, the installer's `PrereqState` scaled to
/// the one tool this flow asks about.
#[derive(Debug, Clone)]
pub struct RequirementState {
    pub requirement: Requirement,
    pub probe: ToolProbe,
    output: String,
    process: Option<ProcessId>,
}

impl RequirementState {
    fn new(requirement: Requirement) -> Self {
        Self {
            requirement,
            probe: ToolProbe::Probing,
            output: String::new(),
            process: None,
        }
    }

    pub fn satisfied(&self) -> bool {
        matches!(self.probe, ToolProbe::Present(_))
    }
}

/// One instrumentation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `sysbuild.conf` carries the MCUboot sysbuild block.
    Sysbuild,
    /// `VERSION` exists and parses.
    Version,
    /// `boards/<target>.conf` carries the OTA Kconfig block.
    BoardConfig,
    /// The opt-in net shell block. [`StepState::Skipped`] by default; the
    /// panel's toggle is the only way in, and its cost rides the row.
    NetShell,
    /// `[ota]` in the project's `chiptui.toml` matches the answers.
    RecordConfig,
}

impl Step {
    /// The sequence, in the order it runs: the config record last, so a
    /// half-prepared project is not *recorded* as prepared.
    pub const ALL: &'static [Step] = &[
        Step::Sysbuild,
        Step::Version,
        Step::BoardConfig,
        Step::NetShell,
        Step::RecordConfig,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Sysbuild => "sysbuild.conf",
            Self::Version => "VERSION",
            Self::BoardConfig => "board Kconfig fragment",
            Self::NetShell => "net shell",
            Self::RecordConfig => "chiptui.toml [ota]",
        }
    }

    /// The tag its guarded block carries, for the two that write one.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::NetShell => "ota-netshell",
            _ => "ota",
        }
    }

    pub const fn optional(self) -> bool {
        matches!(self, Self::NetShell)
    }
}

/// What a run left for the app to act on, mirroring
/// [`crate::install::InstallUpdate`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PrepareUpdate {
    /// Every step reached `Done` or `Skipped`.
    pub finished: bool,
    /// A step failed and stopped the sequence.
    pub stopped: bool,
    /// A line for the log, when something went wrong.
    pub notice: Option<String>,
}

/// The prepare panel's state: a project's OTA instrumentation as five
/// writes and the answers they need.
pub struct Prepare {
    pub root: PathBuf,
    board: String,
    build_dir: Option<String>,
    config: OtaConfig,
    /// One state per [`Step::ALL`] entry, same order.
    pub steps: Vec<StepState>,
    pub slots: SlotCheck,
    /// Whether the build actually carries the transport --- the silent
    /// failure this flow otherwise has no symptom for.
    pub transport: TransportCheck,
    pub requirements: Vec<RequirementState>,
    pub phase: Phase,
    pub output: VecDeque<String>,
    /// Whether the user opted into the net shell block.
    netshell: bool,
    /// The `smpmgr` program --- the test seam, pointed at a fixture.
    tool: String,
}

impl Prepare {
    /// A panel for `root`, with every step's completion already read off
    /// the filesystem: an instrumented project opens showing nothing left
    /// to do, and an interrupted run shows where it stopped.
    pub fn new(
        root: impl Into<PathBuf>,
        board: impl Into<String>,
        build_dir: Option<String>,
        config: OtaConfig,
    ) -> Self {
        let mut panel = Self {
            root: root.into(),
            board: board.into(),
            build_dir,
            config,
            steps: Vec::new(),
            slots: SlotCheck::NotChecked,
            transport: TransportCheck::NotChecked,
            requirements: Requirement::ALL
                .iter()
                .copied()
                .map(RequirementState::new)
                .collect(),
            phase: Phase::Idle,
            output: VecDeque::new(),
            netshell: false,
            tool: super::mcumgr::PROGRAM.to_string(),
        };
        panel.slots = SlotCheck::of(&panel.root, panel.build_dir.as_deref());
        panel.transport = panel.transport_check();
        // The opt-in is read off the project, not assumed off. A project
        // that already carries the block opened with the toggle reading
        // "off" and a heading offering to *add* what was already there,
        // while `s` --- guarded on the step being `Done` --- did nothing at
        // all. Every other step's state comes from the filesystem; so does
        // this answer now.
        panel.netshell = panel.netshell_block_present();
        panel.steps = Step::ALL
            .iter()
            .map(|step| {
                if panel.step_done(*step) {
                    StepState::Done
                } else if step.optional() {
                    StepState::Skipped
                } else {
                    StepState::Pending
                }
            })
            .collect();
        panel
    }

    /// Points the requirement probes at a specific program --- the seam
    /// that keeps tests off `PATH`.
    pub fn set_tool(&mut self, program: impl Into<String>) {
        self.tool = program.into();
    }

    /// The board this panel instruments for.
    pub fn board(&self) -> &str {
        &self.board
    }

    /// The answers being recorded.
    pub fn config(&self) -> &OtaConfig {
        &self.config
    }

    /// Re-answers the config mid-flight (the address entry answered while
    /// the modal was open). The record step's done-ness moves with the
    /// answers it records --- a new address it has not written makes it
    /// pending again; a completed write it still matches stays done.
    pub fn set_config(&mut self, config: OtaConfig) {
        self.config = config;
        self.transport = self.transport_check();
        // Every step, not just the record. This used to re-derive
        // `RecordConfig` alone, which is all an *address* change touches
        // --- but the transport decides the Kconfig body too, so answering
        // it left `board Kconfig fragment` reading `✓` over a block written
        // for a different transport, and the button offered nothing to fix
        // it. `step_done` reads the filesystem, so re-deriving is just
        // asking it again.
        for (index, step) in Step::ALL.iter().enumerate() {
            // A failed step keeps its reason: that is what `Retry` resumes
            // from, and a config change is not an answer to it.
            if matches!(self.steps[index], StepState::Failed(_)) {
                continue;
            }
            self.steps[index] = if *step == Step::NetShell {
                self.netshell_state()
            } else if self.step_done(*step) {
                StepState::Done
            } else {
                StepState::Pending
            };
        }
    }

    /// Whether a failed step stopped the sequence --- the panel's button
    /// then reads "retry" rather than "prepare".
    pub fn stopped(&self) -> bool {
        matches!(self.phase, Phase::Stopped(_))
    }

    /// What a step writes, for its checklist row's value --- the writes
    /// are the commands here, and what runs is never hidden behind a
    /// friendly label (`SPEC.md` §15).
    pub fn step_detail(&self, step: Step) -> String {
        match step {
            Step::Sysbuild => "managed block in sysbuild.conf".to_string(),
            Step::Version => "VERSION, 0.1.0 --- only if absent".to_string(),
            Step::BoardConfig => {
                format!("managed block 'ota' in {}", self.fragment_path().display())
            }
            Step::NetShell => format!(
                "managed block 'ota-netshell' in {} --- {}",
                self.fragment_path().display(),
                NETSHELL_COST
            ),
            Step::RecordConfig => format!("[ota] in {}", config::FILE_NAME),
        }
    }

    /// Whether the net shell block is opted into.
    pub fn netshell(&self) -> bool {
        self.netshell
    }

    /// Whether every requirement is answered.
    pub fn requirements_ready(&self) -> bool {
        self.requirements.iter().all(RequirementState::satisfied)
    }

    /// Whether the sequence can start right now.
    pub fn can_start(&self) -> bool {
        self.requirements_ready() && !self.slots.blocks() && self.next_step().is_some()
    }

    /// The first step still to run, skipping what the filesystem already
    /// shows done and what the user left off. `None` when nothing is left.
    /// A *failed* step counts as still to run: that is what a retry resumes
    /// from.
    pub fn next_step(&self) -> Option<usize> {
        Step::ALL
            .iter()
            .enumerate()
            .find(|(index, _)| {
                matches!(
                    self.steps[*index],
                    StepState::Pending | StepState::Failed(_)
                )
            })
            .map(|(index, _)| index)
    }

    /// What the net shell step reads, given the answer and what is on disk:
    /// written and current is `Done`; opted in without the block, or opted
    /// out with one still there, is work (`Pending`); opted out with
    /// nothing there is `Skipped`.
    fn netshell_state(&self) -> StepState {
        if self.step_done(Step::NetShell) {
            StepState::Done
        } else if self.netshell || self.netshell_block_present() {
            StepState::Pending
        } else {
            StepState::Skipped
        }
    }

    /// Toggles the net shell block: opting in writes it, opting out takes
    /// it back out. Both run under the prepare's own confirm --- `s`
    /// records the answer, the button performs it.
    pub fn toggle_netshell(&mut self) {
        let Some(index) = Step::ALL.iter().position(|step| *step == Step::NetShell) else {
            return;
        };
        self.netshell = !self.netshell;
        // The toggle used to give up whenever the step read `Done`, which
        // is exactly the state a project that already has the block is in
        // --- so the key did nothing while the heading kept offering to add
        // what was already there. It records the answer; the state below
        // says what is left to do about it.
        self.steps[index] = self.netshell_state();
    }

    /// Re-reads the transport precondition, against whichever fragment
    /// this project's board answers to.
    fn transport_check(&self) -> TransportCheck {
        TransportCheck::of(
            &self.root,
            self.build_dir.as_deref(),
            self.config.transport,
            &self.fragment_path(),
        )
    }

    /// Re-reads the slot precondition --- a build that landed after the
    /// panel opened is the way a `NotChecked` becomes an answer.
    pub fn recheck_slots(&mut self) {
        self.slots = SlotCheck::of(&self.root, self.build_dir.as_deref());
        self.transport = self.transport_check();
    }

    /// Starts (or re-probes) every requirement query.
    pub fn probe_requirements(&mut self, processes: &mut ProcessManager) {
        for state in &mut self.requirements {
            if let Some(id) = state.process.take() {
                processes.cancel(id);
            }
            state.probe = ToolProbe::Probing;
            state.output.clear();
            let command = state.requirement.query(&self.tool);
            state.process = Some(processes.spawn(command, PROBE_TIMEOUT));
        }
    }

    /// Feeds a process event back in. Only the requirement probes have
    /// processes --- the steps are synchronous writes --- so every event is
    /// matched against them and everything else is ignored.
    pub fn on_process(&mut self, event: &ProcessEvent) {
        match event {
            ProcessEvent::Line { id, text, .. } | ProcessEvent::Output { id, text } => {
                if let Some(state) = self
                    .requirements
                    .iter_mut()
                    .find(|state| state.process == Some(*id))
                {
                    state.output.push_str(text);
                    state.output.push('\n');
                }
            }
            ProcessEvent::Finished { id, outcome, .. } => {
                let Some(state) = self
                    .requirements
                    .iter_mut()
                    .find(|state| state.process == Some(*id))
                else {
                    return;
                };
                state.process = None;
                state.probe = match outcome {
                    Outcome::SpawnFailed(_) | Outcome::TimedOut | Outcome::Cancelled => {
                        ToolProbe::Missing
                    }
                    // A tool that ran at all is there: a version banner it
                    // worded unusually must not read as missing (the
                    // installer's `Unreadable` rule).
                    Outcome::Success | Outcome::Failed { .. } => {
                        match state.output.lines().find(|line| !line.trim().is_empty()) {
                            Some(line) => ToolProbe::Present(line.trim().to_string()),
                            None => ToolProbe::Present("version unreadable".to_string()),
                        }
                    }
                };
            }
            ProcessEvent::Started { .. } | ProcessEvent::Bytes { .. } => {}
        }
    }

    /// Runs every remaining step, synchronously and in order. A write is
    /// microseconds, so the whole sequence settles inside one call; a
    /// failure stops it there and a retry resumes from the failed step.
    pub fn start(&mut self) -> PrepareUpdate {
        let mut update = PrepareUpdate::default();
        if !self.can_start() {
            return update;
        }
        self.phase = Phase::Running;
        while let Some(index) = self.next_step() {
            let step = Step::ALL[index];
            self.steps[index] = StepState::Running;
            match self.run_step(step) {
                Ok(line) => {
                    // An optional step that ran to *remove* something is
                    // not "done" --- there is nothing there, which is
                    // `Skipped`. Re-deriving beats asserting `Done`.
                    self.steps[index] = if step.optional() && !self.step_done(step) {
                        StepState::Skipped
                    } else {
                        StepState::Done
                    };
                    self.push_output(line);
                }
                Err(reason) => {
                    let reason = format!("{}: {reason}", step.label());
                    self.steps[index] = StepState::Failed(reason.clone());
                    self.phase = Phase::Stopped(reason);
                    update.stopped = true;
                    update.notice = Some(format!("OTA prepare: {}", self.stop_reason()));
                    return update;
                }
            }
        }
        self.phase = Phase::Finished;
        // The run just rewrote the fragment, so any `.config` on disk is
        // now provably older than it: re-read rather than leave a verdict
        // the writes invalidated.
        self.transport = self.transport_check();
        update.finished = true;
        update
    }

    pub fn stop_reason(&self) -> String {
        match &self.phase {
            Phase::Stopped(reason) => reason.clone(),
            _ => String::new(),
        }
    }

    /// One step's write, as the log line describing what happened.
    fn run_step(&self, step: Step) -> Result<String, String> {
        match step {
            Step::Sysbuild => self
                .apply(scaffold::GuardedBlock {
                    path: PathBuf::from("sysbuild.conf"),
                    tag: step.tag(),
                    body: SYSBUILD_BODY.to_string(),
                })
                .map(|applied| format!("write sysbuild.conf --- {}", describe(applied))),
            Step::Version => {
                let path = self.root.join("VERSION");
                match std::fs::read_to_string(&path) {
                    Ok(text) if version_parses(&text) => {
                        Ok("VERSION --- already there and parses".to_string())
                    }
                    Ok(_) => Err(
                        "VERSION exists but does not parse (VERSION_MAJOR, VERSION_MINOR, \
                         PATCHLEVEL and VERSION_TWEAK, each 0-255) --- refusing to overwrite \
                         it; fix it by hand"
                            .to_string(),
                    ),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        crate::settings::write_config(&path, VERSION_BODY)
                            .map_err(|err| format!("cannot write VERSION: {err}"))?;
                        Ok("write VERSION --- created (0.1.0)".to_string())
                    }
                    Err(err) => Err(format!("cannot read VERSION: {err}")),
                }
            }
            Step::BoardConfig => self
                .apply(scaffold::GuardedBlock {
                    path: self.fragment_path(),
                    tag: step.tag(),
                    body: kconfig_body(self.config.transport),
                })
                .map(|applied| {
                    format!(
                        "write {} --- {}",
                        self.fragment_path().display(),
                        describe(applied)
                    )
                }),
            // The one step that runs in both directions: the guarded block
            // makes removal exact, so opting back out is a real answer
            // rather than a key that silently does nothing. It runs under
            // the prepare's own confirm --- `s` records the answer, the
            // button performs it, because `s` must not write.
            Step::NetShell if self.netshell => self
                .apply(scaffold::GuardedBlock {
                    path: self.fragment_path(),
                    tag: step.tag(),
                    body: NETSHELL_BODY.to_string(),
                })
                .map(|applied| {
                    format!(
                        "write {} --- {}",
                        self.fragment_path().display(),
                        describe(applied)
                    )
                }),
            Step::NetShell => self.remove(step.tag()).map(|_| {
                format!(
                    "write {} --- net shell block removed",
                    self.fragment_path().display()
                )
            }),
            Step::RecordConfig => {
                config::save_ota(&self.root.join(config::FILE_NAME), &self.config)
                    .map_err(|err| format!("cannot write {}: {err}", config::FILE_NAME))?;
                Ok(format!(
                    "write {} --- [ota] method={} transport={}",
                    config::FILE_NAME,
                    self.config.method.id(),
                    self.config.transport.id()
                ))
            }
        }
    }

    /// The fragment this project's board answers to (relative to the
    /// root): the existing file in either spelling, else where a new one
    /// goes.
    fn fragment_path(&self) -> PathBuf {
        variants::fragment_path(&self.root, &self.board)
            .unwrap_or_else(|| variants::fragment_path_for(&self.board))
    }

    /// Whether the fragment currently carries the net shell block.
    fn netshell_block_present(&self) -> bool {
        std::fs::read_to_string(self.root.join(self.fragment_path()))
            .is_ok_and(|text| matches!(scaffold::find_tag(&text, Step::NetShell.tag()), Ok(true)))
    }

    /// Takes a guarded block back out of the fragment, leaving every byte
    /// outside its markers as it was.
    fn remove(&self, tag: &str) -> Result<(), String> {
        let path = self.fragment_path();
        let full = self.root.join(&path);
        let Ok(text) = std::fs::read_to_string(&full) else {
            // Nothing there is nothing to remove.
            return Ok(());
        };
        let updated = scaffold::remove_block(&text, tag)
            .map_err(|reason| format!("cannot rewrite {}: {reason}", path.display()))?;
        if updated == text {
            return Ok(());
        }
        crate::settings::write_config(&full, &updated)
            .map_err(|err| format!("cannot write {}: {err}", path.display()))
    }

    fn apply(&self, block: scaffold::GuardedBlock) -> Result<scaffold::Applied, String> {
        scaffold::apply_block(&self.root, &block)
            .map_err(|err| format!("cannot write {}: {err}", block.path.display()))
    }

    /// Whether the filesystem already shows a step done --- the resume
    /// predicate, read off content and never off a record of a previous
    /// run.
    fn step_done(&self, step: Step) -> bool {
        match step {
            Step::Sysbuild => std::fs::read_to_string(self.root.join("sysbuild.conf"))
                .is_ok_and(|text| has_symbol(&text, "SB_CONFIG_BOOTLOADER_MCUBOOT")),
            Step::Version => std::fs::read_to_string(self.root.join("VERSION"))
                .is_ok_and(|text| version_parses(&text)),
            Step::BoardConfig => std::fs::read_to_string(self.root.join(self.fragment_path()))
                .is_ok_and(|text| {
                    scaffold::block_matches(&text, step.tag(), &kconfig_body(self.config.transport))
                }),
            // Only "opted in and already current" is done. Opted *out* is
            // not a completion --- it is either nothing to do (the step
            // reads `Skipped`) or a removal still owed (`Pending`), and
            // `toggle_netshell` is what tells those apart.
            Step::NetShell => {
                self.netshell
                    && std::fs::read_to_string(self.root.join(self.fragment_path()))
                        .is_ok_and(|text| scaffold::block_matches(&text, step.tag(), NETSHELL_BODY))
            }
            Step::RecordConfig => std::fs::read_to_string(self.root.join(config::FILE_NAME))
                .ok()
                .and_then(|text| config::parse_ota(&text))
                .is_some_and(|recorded| {
                    recorded.method == self.config.method
                        && recorded.transport == self.config.transport
                        // An unanswered address is never written, so it
                        // cannot be a mismatch either.
                        && self
                            .config
                            .address
                            .as_ref()
                            .is_none_or(|address| recorded.address.as_ref() == Some(address))
                }),
        }
    }

    fn push_output(&mut self, line: String) {
        if self.output.len() >= OUTPUT_CAPACITY {
            self.output.pop_front();
        }
        self.output.push_back(line);
    }
}

fn describe(applied: scaffold::Applied) -> &'static str {
    match applied {
        scaffold::Applied::Created => "created",
        scaffold::Applied::Appended => "appended the managed block",
        scaffold::Applied::Replaced => "updated the managed block",
        scaffold::Applied::Unchanged => "already done",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kconfig_body_carries_only_verified_symbols() {
        // Verified against the Zephyr tree's own Kconfig files; a rename
        // upstream is caught here rather than in someone's build.
        for transport in Transport::ALL {
            let body = kconfig_body(*transport);
            assert!(
                body.contains("CONFIG_BOOTLOADER_MCUBOOT=y"),
                "{transport:?}"
            );
            assert!(body.contains("CONFIG_MCUMGR=y"));
            assert!(body.contains("CONFIG_MCUMGR_GRP_IMG=y"));
            assert!(body.contains("CONFIG_MCUMGR_GRP_OS=y"));
            assert!(body.contains("CONFIG_MCUMGR_GRP_OS_MCUMGR_PARAMS=y"));
            assert!(body.contains("CONFIG_REBOOT=y"));
            assert!(body.contains("CONFIG_IMG_MANAGER=y"));
            assert!(body.contains("CONFIG_IMG_ERASE_PROGRESSIVELY=y"));
            assert!(body.contains("CONFIG_STREAM_FLASH=y"));
            assert!(body.contains("CONFIG_ZCBOR=y"));
            assert!(body.contains("CONFIG_NET_BUF=y"));
            assert!(body.contains("CONFIG_CRC=y"));
        }
        assert!(kconfig_body(Transport::Udp).contains("CONFIG_MCUMGR_TRANSPORT_UDP=y"));
        assert!(kconfig_body(Transport::Udp).contains("CONFIG_MCUMGR_TRANSPORT_UDP_IPV4=y"));
        // Serial's symbol really is UART, not SERIAL.
        assert!(kconfig_body(Transport::Serial).contains("CONFIG_MCUMGR_TRANSPORT_UART=y"));
        assert!(kconfig_body(Transport::Ble).contains("CONFIG_MCUMGR_TRANSPORT_BT=y"));
        assert!(kconfig_body(Transport::Ble).contains("CONFIG_BT_PERIPHERAL=y"));
    }

    #[test]
    fn situational_tweaks_are_comments_with_reasons_and_udp_only() {
        let udp = kconfig_body(Transport::Udp);
        assert!(udp.contains("# CONFIG_NET_MAX_CONN=8"));
        assert!(udp.contains("# CONFIG_NET_IPV6=n"));
        assert!(
            !udp.lines().any(|line| line == "CONFIG_NET_MAX_CONN=8"),
            "commented, not set"
        );
        // Networking tweaks are noise on a transport that has no network.
        assert!(!kconfig_body(Transport::Serial).contains("NET_MAX_CONN"));
        assert!(!kconfig_body(Transport::Ble).contains("NET_IPV6"));
    }

    #[test]
    fn has_symbol_reads_assignments_not_mentions() {
        assert!(has_symbol("CONFIG_X=y\n", "CONFIG_X"));
        assert!(has_symbol("CONFIG_X = y\n", "CONFIG_X"));
        assert!(!has_symbol("# CONFIG_X=y\n", "CONFIG_X"));
        assert!(!has_symbol("#CONFIG_X=y\n", "CONFIG_X"));
        assert!(!has_symbol("CONFIG_X=n\n", "CONFIG_X"));
        assert!(!has_symbol("CONFIG_X_EXTRA=y\n", "CONFIG_X"));
        assert!(!has_symbol("CONFIG_X=yesterday\n", "CONFIG_X"));
    }

    #[test]
    fn version_parses_is_the_builds_own_contract() {
        assert!(version_parses(VERSION_BODY));
        assert!(version_parses(
            "VERSION_MAJOR = 1\nVERSION_MINOR = 2\nPATCHLEVEL = 3\nVERSION_TWEAK = 4\nEXTRAVERSION = rc1\n"
        ));
        // Every numeric field is mandatory in Zephyr's versioning --- a
        // missing one is a FATAL_ERROR at configure time.
        assert!(!version_parses(
            "VERSION_MAJOR = 1\nVERSION_MINOR = 2\nPATCHLEVEL = 3\n"
        ));
        assert!(!version_parses(
            "VERSION_MAJOR = 999\nVERSION_MINOR = 0\nPATCHLEVEL = 0\nVERSION_TWEAK = 0\n"
        ));
        assert!(!version_parses(
            "VERSION_MAJOR = one\nVERSION_MINOR = 0\nPATCHLEVEL = 0\nVERSION_TWEAK = 0\n"
        ));
        assert!(!version_parses(""));
    }

    fn temp_project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-prepare-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `zephyr.dts` with the three nodes an A/B layout needs, in the
    /// shape `devicetree::parse` reads --- the `/* node '<path>' */`
    /// annotations are what the parser builds paths from, and only a node
    /// under a `partitions` path counts. The full fixture with and without
    /// them lives in `tests/ota_prepare.rs`.
    const DTS_WITH_SLOTS: &str = "\
/* node '/soc/flash@0/partitions' defined in board.dtsi:10 */
partitions {
        /* node '/soc/flash@0/partitions/partition@0' defined in board.dtsi:13 */
        boot_partition: partition@0 {
                label = \"mcuboot\";
                reg = < 0x0 0x10000 >;
        };
        /* node '/soc/flash@0/partitions/partition@20000' defined in board.dtsi:25 */
        slot0_partition: partition@20000 {
                label = \"image-0\";
                reg = < 0x20000 0x1c0000 >;
        };
        /* node '/soc/flash@0/partitions/partition@1e0000' defined in board.dtsi:31 */
        slot1_partition: partition@1e0000 {
                label = \"image-1\";
                reg = < 0x1e0000 0x1c0000 >;
        };
};
";

    #[test]
    fn the_slot_check_has_three_honest_states() {
        let root = temp_project("slots");
        // Never built: no directory, nothing to read --- and non-blocking.
        assert_eq!(SlotCheck::of(&root, None), SlotCheck::NotChecked);
        assert_eq!(SlotCheck::of(&root, Some("build")), SlotCheck::NotChecked);
        assert!(!SlotCheck::NotChecked.blocks());

        // Built, slots present.
        let dts = root.join("build/zephyr/zephyr.dts");
        std::fs::create_dir_all(dts.parent().unwrap()).unwrap();
        std::fs::write(&dts, DTS_WITH_SLOTS).unwrap();
        assert_eq!(
            SlotCheck::of(&root, Some("build")),
            SlotCheck::Found(dts.clone())
        );

        // Built, slot1 absent: the refusal names the missing nodes.
        std::fs::write(
            &dts,
            DTS_WITH_SLOTS.replace("slot1_partition", "storage_partition"),
        )
        .unwrap();
        match SlotCheck::of(&root, Some("build")) {
            SlotCheck::Missing(missing, path) => {
                assert_eq!(missing, vec!["slot1_partition"]);
                assert_eq!(path, dts);
            }
            other => panic!("expected Missing, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
