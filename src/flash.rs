//! `esptool` flash/erase panel state.
//!
//! Parallel to [`crate::browser::Browser`]: emits [`Notice`] values and a
//! [`FlashUpdate`] the caller forwards, never touches the log or the
//! terminal directly, which keeps the whole state machine testable without a
//! UI. Unlike the browser, only one command is ever meaningful at a time
//! (the user explicitly picks an action), so there is no request queue.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::backend::micropython::curl::{commands as curl_commands, parse as curl_parse};
use crate::backend::micropython::esptool::{
    ChipFamily, DeviceDetails, FlashFreq, FlashMode, FlashOptions, FlashSize, commands, parse,
};
use crate::backend::micropython::firmware::{self, BoardCandidate, FirmwareFile, FirmwareKind};
use crate::backend::tool_available;
use crate::files::{self, LocalEntry};
use crate::firmware_id::{self, FirmwareVerdict, FlashFirmware};
use crate::logs::Level;
use crate::process::{Outcome, ProcessEvent, ProcessId, ProcessManager, Stream};

/// esptool operations can run for minutes on a large image.
pub const FLASH_TIMEOUT: Duration = Duration::from_secs(180);
/// Fetching a search/board page should be quick; a generous ceiling still
/// beats hanging the panel on a slow or stalled connection.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// A firmware image can be several megabytes over a slow connection.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

/// A message for the log pane.
pub type Notice = (Level, String);

/// One entry of the flash menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashAction {
    ChipInfo,
    FlashInfo,
    EraseFlash,
    WriteFlash,
    VerifyFlash,
    Reset,
    /// Reads the partition table plus the start of the app area so
    /// [`crate::firmware_id`] can identify the installed firmware.
    /// Background-only: it is deliberately absent from [`Self::ALL`] --- the
    /// user reaches it through the identification question, never the menu,
    /// because it stops the running firmware (esptool resets the board to
    /// read its flash) and that consent is collected by the caller.
    ReadFlash,
}

impl FlashAction {
    pub const ALL: &'static [FlashAction] = &[
        Self::ChipInfo,
        Self::FlashInfo,
        Self::EraseFlash,
        Self::WriteFlash,
        Self::VerifyFlash,
        Self::Reset,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ChipInfo => "Chip information",
            Self::FlashInfo => "Flash information",
            Self::EraseFlash => "Erase flash",
            Self::WriteFlash => "Write / flash firmware",
            Self::VerifyFlash => "Verify flash",
            Self::Reset => "Reset",
            Self::ReadFlash => "Identify firmware",
        }
    }

    /// Single-glyph marker for the flash menu, same monochrome-unicode
    /// convention as [`crate::files::SyncStatus::marker`] --- colored by the
    /// terminal, not by an emoji font.
    pub const fn icon(self) -> &'static str {
        match self {
            Self::ChipInfo => "◆",
            Self::FlashInfo => "▦",
            Self::EraseFlash => "⌫",
            Self::WriteFlash => "⇪",
            Self::VerifyFlash => "✓",
            Self::Reset => "↺",
            Self::ReadFlash => "◎",
        }
    }

    /// `SPEC.md` §15: writing to or erasing flash always requires confirmation.
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::EraseFlash | Self::WriteFlash)
    }

    /// Whether a firmware file must be chosen before this action can run.
    pub const fn needs_firmware(self) -> bool {
        matches!(self, Self::WriteFlash | Self::VerifyFlash)
    }
}

/// One row of the device pane's **Project actions** tab (the tab the
/// flash menu became): an esptool action, the online-firmware search, or
/// --- appended exactly while a command runs, never a row of the stack ---
/// `Stop`. The same shape [`crate::build::BuildAction`] gives the build
/// pane, so the two panes read identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashPaneAction {
    Run(FlashAction),
    /// Searches micropython.org/download/ for the known chip
    /// (`FlashPanel::search_online`), as a button instead of the menu's
    /// old `s` key. A direct download URL has no button of its own: the
    /// search results' `u` key is the way to [`FlashScreen::CustomUrl`].
    SearchOnline,
    Stop,
}

/// What the panel currently knows about the connected chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipGuess {
    Unknown,
    /// Read from an esptool banner.
    Detected(ChipFamily),
    /// Picked by the user; a later detection must not silently replace it.
    Overridden(ChipFamily),
}

impl ChipGuess {
    pub fn family(self) -> Option<ChipFamily> {
        match self {
            Self::Unknown => None,
            Self::Detected(family) | Self::Overridden(family) => Some(family),
        }
    }
}

/// Which screen of the flash view is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashScreen {
    Menu,
    /// Offset / chip / flash-mode / flash-freq / flash-size / custom flags.
    Options,
    /// Boards found by [`FlashPanel::search_online`].
    OnlineBoards,
    /// Firmware builds found for the board picked from [`Self::OnlineBoards`].
    OnlineFirmware,
    /// Free-text entry for a direct firmware download URL, bypassing search.
    CustomUrl,
}

/// A field on the options screen. `WriteFlash` offers all of them;
/// `VerifyFlash` only needs the chip and offset (`commands::verify_flash`
/// takes no [`FlashOptions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionsField {
    Chip,
    Offset,
    FlashMode,
    FlashFreq,
    FlashSize,
    ExtraArgs,
}

const WRITE_FIELDS: &[OptionsField] = &[
    OptionsField::Chip,
    OptionsField::Offset,
    OptionsField::FlashMode,
    OptionsField::FlashFreq,
    OptionsField::FlashSize,
    OptionsField::ExtraArgs,
];
const VERIFY_FIELDS: &[OptionsField] = &[OptionsField::Chip, OptionsField::Offset];

/// The outcome of the most recently finished command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RunState {
    #[default]
    Idle,
    Running,
    Succeeded,
    Failed(String),
}

/// What the panel is busy with, for the actions tab's state line
/// ([`FlashPanel::activity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// A command the user started --- from the tab's own buttons or from
    /// a dialog. The only one counted while it runs, and the only one
    /// that finishes into a [`FlashReport`]; the row it started from
    /// names it, so the state line does not have to.
    User,
    /// One of the background `esptool` queries: the chip identity, the
    /// firmware identification read, the version hunt. Courtesy work the
    /// user never asked to watch, but it holds the port, so the pane says
    /// so instead of claiming there is nothing running.
    Query,
    /// The online board/firmware search ([`FlashPanel::search_online`],
    /// [`FlashPanel::fetch_board_page`]).
    Search,
    /// A firmware download ([`FlashPanel::download`]).
    Download,
}

/// Result of the last finished *user-started* command, for the actions
/// tab's state line --- the flash pane's counterpart of
/// [`crate::build::BuildReport`]. Background queries (the courtesy chip
/// refresh, the identification read) never report here: they are not work
/// the user asked to watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashReport {
    /// What ran, as the action's own label ("erase flash", …).
    pub what: &'static str,
    pub ok: bool,
    pub duration: Duration,
}

/// stdout/stderr collected for the in-flight process, kept separate from
/// [`FlashPanel::output`] because [`parse::parse_chip_family`],
/// [`parse::parse_device_details`] and [`parse::explain_error`] need
/// whole-stream text, not per-line display.
struct RunningCommand {
    id: ProcessId,
    action: FlashAction,
    /// True when spawned by [`FlashPanel::query_device_info`]'s courtesy
    /// refresh rather than a user-initiated [`FlashPanel::run`] --- only the
    /// latter should ever trigger an unprompted online firmware search.
    background: bool,
    /// Set only for [`FlashAction::ReadFlash`]: where `esptool read-flash`
    /// is writing the bytes [`complete`] will parse.
    probe_dest: Option<PathBuf>,
    /// True when this read is the version hunt
    /// ([`FlashPanel::query_firmware_version`]) rather than the
    /// identification read: it reads the follow-up window
    /// ([`firmware_id::HUNT_OFFSET`]) and its finish must not re-drive the
    /// first-listing chain.
    hunt_version: bool,
    started: Instant,
    stdout: String,
    stderr: String,
    /// The latest step/percentage parsed out of the command's own output
    /// (see [`crate::progress`]) --- esptool's write-flash percentage, most
    /// commonly. `None` until a matching line arrives, which most esptool
    /// actions (chip/flash info, erase, reset) never send.
    progress: Option<crate::progress::Progress>,
}

pub struct FlashPanel {
    /// The project's `firmware/` directory --- where online downloads land
    /// and where [`FlashPanel::discover_firmware`] looks (`SPEC.md` §9).
    pub firmware_dir: PathBuf,
    /// Cursor into [`FlashAction::ALL`].
    pub cursor: usize,
    /// Cursor into the Project actions tab's rows
    /// ([`FlashPanel::pane_actions`]) --- the pane the flash menu became,
    /// navigated like the build pane's list.
    pub pane_cursor: usize,
    pub screen: FlashScreen,
    pub firmware: Vec<LocalEntry>,
    pub selected_firmware: Option<usize>,
    pub chip: ChipGuess,
    /// Everything esptool has reported about the connected board so far,
    /// accumulated across runs (`SPEC.md`'s device panel). Independent of
    /// [`Self::chip`]: this is never manually overridden, only ever grown
    /// from real esptool output.
    pub details: DeviceDetails,
    pub offset: String,
    /// Once the user edits the offset by hand, chip detection must stop
    /// overwriting it.
    offset_touched: bool,
    pub options: FlashOptions,
    pub options_focus: OptionsField,
    /// Lines from the current or most recently finished run, in arrival order.
    pub output: Vec<String>,
    pub state: RunState,
    /// The last finished user-started command, for the actions tab's
    /// state line (see [`FlashReport`]).
    pub last: Option<FlashReport>,
    /// Set by [`FlashPanel::request_confirmation`], consumed by the caller
    /// once the user accepts the confirmation overlay.
    pending_action: Option<FlashAction>,
    in_flight: Option<RunningCommand>,
    /// Whether the last identification read left a firmware named without
    /// a version --- the version hunt
    /// ([`FlashPanel::query_firmware_version`]) is armed for it, and the
    /// caller runs it through the same tick-polled deferral as the other
    /// background queries. Cleared with [`Self::details`]: a verdict that
    /// no longer stands must not be filled in after the fact.
    version_hunt_pending: bool,
    /// Overrides the `esptool` executable. `None` means "resolve on PATH".
    tool_path: Option<String>,
    /// Boards found by [`FlashPanel::search_online`].
    pub online_boards: Vec<BoardCandidate>,
    /// Firmware builds found for the board picked from [`Self::online_boards`].
    pub online_firmware: Vec<FirmwareFile>,
    /// The URL the currently shown online list came from (or is being
    /// fetched from) --- rendered by the flash view so the source of a
    /// search is always visible, never a mystery feed (`SPEC.md` §9).
    pub online_source: Option<String>,
    /// Cursor shared by [`Self::online_boards`]/[`Self::online_firmware`] ---
    /// only one of the two is ever on screen at a time.
    pub online_cursor: usize,
    /// Free-text buffer for [`FlashScreen::CustomUrl`].
    pub custom_url: String,
    /// The one curl fetch/download in flight, independent of
    /// [`Self::in_flight`] so an esptool action and an online search/download
    /// can never race each other (both are still mutually exclusive, since
    /// [`FlashPanel::is_busy`] and [`FlashPanel::run`]'s guard check both).
    in_flight_fetch: Option<RunningFetch>,
    /// Overrides the `curl` executable. `None` means "resolve on PATH".
    curl_tool_path: Option<String>,
}

/// stdout collected for the in-flight curl fetch/download. Unlike
/// [`RunningCommand`], stderr is not tracked separately: curl's `-sS` keeps
/// it to genuine errors, which the log pane shows as-is.
struct RunningFetch {
    id: ProcessId,
    kind: FetchKind,
    stdout: String,
}

enum FetchKind {
    Boards,
    Firmware { board_id: String },
    Download { dest: PathBuf },
}

/// A unique scratch path for one `read-flash` run, under the system temp
/// dir --- never the project tree, and never a fixed name (tests run in
/// parallel threads that would share a pid).
fn firmware_probe_path() -> PathBuf {
    static PROBE_COUNT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "chiptui-firmware-probe-{}-{}.bin",
        std::process::id(),
        PROBE_COUNT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// What an online search/download changed.
#[derive(Default)]
pub struct FlashFetchUpdate {
    pub notices: Vec<Notice>,
    /// A firmware file just finished downloading successfully; the caller
    /// drives the post-download erase/flash offer.
    pub download_finished: bool,
}

/// What a process event changed.
#[derive(Default)]
pub struct FlashUpdate {
    pub notices: Vec<Notice>,
    /// An erase just finished successfully and firmware discovery already
    /// ran; the caller decides whether to open a picker or the options
    /// screen. Flashing itself never happens automatically --- it still
    /// needs its own confirmation (`SPEC.md` §15).
    pub offer_flash: bool,
    /// Device query finished, chip is known, but firmware folder is empty.
    pub search_online_for_firmware: bool,
    /// The background chip query ([`FlashPanel::query_device_info`])
    /// finished --- successfully or not. Whatever was chained behind it
    /// (the firmware-identification read, when `App` armed one) may
    /// proceed.
    pub background_chip_query_finished: bool,
    /// The background firmware-identification read
    /// ([`FlashPanel::query_firmware_identity`]) finished: its verdict (or
    /// lack of one) is in [`FlashPanel::details`], and the first device
    /// listing waiting on it can be judged against it.
    pub background_firmware_read_finished: bool,
    /// An erase or write-flash just succeeded, so whatever firmware the
    /// identification read named is stale: the caller drops the verdict
    /// (and its gate) so the next listing re-identifies.
    pub firmware_invalidated: bool,
}

impl FlashPanel {
    pub fn new(firmware_dir: impl Into<PathBuf>) -> Self {
        Self {
            firmware_dir: firmware_dir.into(),
            cursor: 0,
            pane_cursor: 0,
            screen: FlashScreen::Menu,
            firmware: Vec::new(),
            selected_firmware: None,
            chip: ChipGuess::Unknown,
            details: DeviceDetails::default(),
            offset: String::new(),
            offset_touched: false,
            options: FlashOptions::default(),
            options_focus: OptionsField::Chip,
            output: Vec::new(),
            state: RunState::default(),
            last: None,
            pending_action: None,
            in_flight: None,
            version_hunt_pending: false,
            tool_path: None,
            online_boards: Vec::new(),
            online_firmware: Vec::new(),
            online_source: None,
            online_cursor: 0,
            custom_url: String::new(),
            in_flight_fetch: None,
            curl_tool_path: None,
        }
    }

    pub fn set_tool_path(&mut self, program: impl Into<String>) {
        self.tool_path = Some(program.into());
    }

    pub fn set_curl_tool_path(&mut self, program: impl Into<String>) {
        self.curl_tool_path = Some(program.into());
    }

    /// Drops everything esptool has reported so far. [`DeviceDetails::merge`]
    /// is deliberately additive-only, so a disconnect (`App::on_process`'s
    /// empty-scan branch) is the only thing that should ever clear this ---
    /// otherwise the Dashboard's Device panel keeps showing the previous
    /// board's chip/flash identity after it is gone.
    pub fn clear_device_details(&mut self) {
        self.details = DeviceDetails::default();
        self.version_hunt_pending = false;
    }

    /// Drops only the firmware identification. Called when the device
    /// selection changes: the answer belongs to the board it was read
    /// from, and unlike the identity fields (refreshed by the next chip
    /// query) it only comes back once the identification question has been
    /// answered for the new board --- until then `undefined` is the honest
    /// display, not the previous board's answer.
    pub fn clear_firmware_identity(&mut self) {
        self.details.firmware = None;
        self.version_hunt_pending = false;
    }

    pub fn selected_action(&self) -> FlashAction {
        FlashAction::ALL[self.cursor]
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = FlashAction::ALL.len() as isize;
        self.cursor = (self.cursor as isize + delta).rem_euclid(len) as usize;
    }

    /// Points the menu cursor at `action`, so [`FlashPanel::selected_action`]
    /// reports it even when the user did not navigate there themselves (the
    /// post-erase flash offer).
    pub fn set_cursor_to(&mut self, action: FlashAction) {
        if let Some(index) = FlashAction::ALL.iter().position(|a| *a == action) {
            self.cursor = index;
        }
    }

    /// The button rows the device pane's Project actions tab shows: the
    /// esptool menu actions in their order, then the online-firmware
    /// search, with `Stop` appended exactly while a command runs --- drawn
    /// as its own half-width box, never a stack row, like the build pane's
    /// own `Stop`.
    ///
    /// [`FlashAction::ChipInfo`] is not among them: the identity it reads
    /// is already asked in the background of every device selection
    /// ([`Self::query_device_info`]) and shown in the Device info pane, so
    /// a button for it would only re-run work the pane has done.
    pub fn pane_actions(&self) -> Vec<FlashPaneAction> {
        let mut rows: Vec<FlashPaneAction> = FlashAction::ALL
            .iter()
            .filter(|action| **action != FlashAction::ChipInfo)
            .map(|action| FlashPaneAction::Run(*action))
            .collect();
        rows.push(FlashPaneAction::SearchOnline);
        if self.is_busy() {
            rows.push(FlashPaneAction::Stop);
        }
        rows
    }

    /// The row at `index` in the tab's drawn list, mirroring the layout
    /// [`Self::pane_actions`] describes (bounds-checked: a cursor left on
    /// `Stop` by a finishing command points past the shrunken list until
    /// `complete` moves it).
    pub fn pane_action_at(&self, index: usize) -> Option<FlashPaneAction> {
        self.pane_actions().into_iter().nth(index)
    }

    /// Points the tab's cursor at `action`'s row: a finished command lands
    /// back where it started (the build panel's post-finish tour).
    pub fn set_pane_cursor_to(&mut self, action: FlashAction) {
        if let Some(index) = self
            .pane_actions()
            .iter()
            .position(|row| matches!(row, FlashPaneAction::Run(candidate) if *candidate == action))
        {
            self.pane_cursor = index;
        }
    }

    /// Elapsed time of the running command, for the tab's live counter
    /// (build pane's rule: drawn only while a *user* command runs).
    pub fn elapsed(&self) -> Option<Duration> {
        self.in_flight
            .as_ref()
            .map(|running| running.started.elapsed())
    }

    /// The running command's own label (`FlashAction::label`), for the
    /// state line's progress text --- a percentage alone does not say what
    /// it is a percentage of.
    pub fn running_label(&self) -> Option<&'static str> {
        self.in_flight
            .as_ref()
            .map(|running| running.action.label())
    }

    /// The running command's latest parsed progress (see [`crate::progress`]).
    /// `None` either before the first matching line arrives or once the
    /// command finishes.
    pub fn progress(&self) -> Option<crate::progress::Progress> {
        self.in_flight.as_ref().and_then(|running| running.progress)
    }

    /// What is holding the panel right now, as the actions tab's state
    /// line reports it.
    ///
    /// Every one of these dims the tab's buttons --- one `esptool` at a
    /// time, one fetch at a time --- so the pane names all three rather
    /// than leaving a dimmed menu unexplained. Only [`Activity::User`] is
    /// the user's *work*, though: it alone is counted, and it alone
    /// finishes into a [`FlashReport`].
    pub fn activity(&self) -> Option<Activity> {
        if let Some(running) = &self.in_flight {
            return Some(if running.background {
                Activity::Query
            } else {
                Activity::User
            });
        }
        match self.fetch_kind()? {
            FetchKind::Boards | FetchKind::Firmware { .. } => Some(Activity::Search),
            FetchKind::Download { .. } => Some(Activity::Download),
        }
    }

    /// Cancels whatever is running at the user's request (the tab's
    /// `Stop`), build-panel rule: takes effect within the process
    /// manager's poll interval.
    ///
    /// Whatever, not just the esptool command: a `Stop` the tab offers for
    /// a curl fetch --- and it offers one, since [`Self::is_busy`] is what
    /// puts the row there --- has to reach it, or the pane sits with every
    /// button dimmed behind a button that does nothing.
    pub fn stop(&mut self, processes: &mut ProcessManager) -> bool {
        let mut stopped = false;
        if let Some(running) = &self.in_flight {
            processes.cancel(running.id);
            stopped = true;
        }
        if let Some(fetch) = &self.in_flight_fetch {
            processes.cancel(fetch.id);
            stopped = true;
        }
        stopped
    }

    /// Scans `firmware_dir` for `.bin`/`.elf` candidates.
    ///
    /// A single match selects itself, same convention as a lone device
    /// (`DeviceState::set_devices`): asking the user to choose from a list of
    /// one is noise. With none, or several, the caller is told so it can warn
    /// or open a picker.
    pub fn discover_firmware(&mut self) -> Vec<Notice> {
        match files::firmware_candidates(&self.firmware_dir) {
            Ok(entries) => {
                let notice = match entries.len() {
                    0 => vec![(
                        Level::Warn,
                        format!(
                            "no .bin/.elf firmware found in {}",
                            self.firmware_dir.display()
                        ),
                    )],
                    1 => vec![(Level::Info, format!("found firmware: {}", entries[0].name))],
                    count => vec![(Level::Info, format!("found {count} firmware files"))],
                };
                self.selected_firmware = if entries.len() == 1 { Some(0) } else { None };
                self.firmware = entries;
                notice
            }
            Err(error) => {
                self.firmware.clear();
                self.selected_firmware = None;
                vec![(
                    Level::Error,
                    format!("cannot read {}: {error}", self.firmware_dir.display()),
                )]
            }
        }
    }

    pub fn select_firmware(&mut self, index: usize) -> bool {
        if index < self.firmware.len() {
            self.selected_firmware = Some(index);
            true
        } else {
            false
        }
    }

    pub fn selected_firmware_path(&self) -> Option<PathBuf> {
        let entry = self.firmware.get(self.selected_firmware?)?;
        Some(self.firmware_dir.join(&entry.name))
    }

    pub fn set_offset(&mut self, offset: String) {
        self.offset = offset;
        self.offset_touched = true;
    }

    pub fn push_offset_char(&mut self, c: char) {
        self.offset.push(c);
        self.offset_touched = true;
    }

    pub fn backspace_offset(&mut self) {
        self.offset.pop();
        self.offset_touched = true;
    }

    pub fn push_extra_arg_char(&mut self, c: char) {
        self.options.extra_args.push(c);
    }

    pub fn backspace_extra_args(&mut self) {
        self.options.extra_args.pop();
    }

    /// Manual override, always available regardless of whether detection has
    /// run (`SPEC.md` §8's "never guess, always allow an override" applied to
    /// chip selection).
    pub fn cycle_chip(&mut self, forward: bool) {
        let family = cycle(ChipFamily::ALL, self.chip.family(), forward);
        self.chip = ChipGuess::Overridden(family);
        self.apply_default_offset(family);
    }

    pub fn cycle_flash_mode(&mut self, forward: bool) {
        self.options.flash_mode = Some(cycle(FlashMode::ALL, self.options.flash_mode, forward));
    }

    pub fn cycle_flash_freq(&mut self, forward: bool) {
        self.options.flash_freq = Some(cycle(FlashFreq::ALL, self.options.flash_freq, forward));
    }

    pub fn cycle_flash_size(&mut self, forward: bool) {
        self.options.flash_size = Some(cycle(FlashSize::ALL, self.options.flash_size, forward));
    }

    /// The fields the options screen shows for `action` --- `VerifyFlash`
    /// takes no [`FlashOptions`], so only the chip and offset are relevant.
    pub fn options_fields(action: FlashAction) -> &'static [OptionsField] {
        if action == FlashAction::WriteFlash {
            WRITE_FIELDS
        } else {
            VERIFY_FIELDS
        }
    }

    pub fn step_options_focus(&mut self, action: FlashAction, forward: bool) {
        let fields = Self::options_fields(action);
        let index = fields
            .iter()
            .position(|field| *field == self.options_focus)
            .unwrap_or(0);
        let len = fields.len() as isize;
        let next = (index as isize + if forward { 1 } else { -1 }).rem_euclid(len) as usize;
        self.options_focus = fields[next];
    }

    /// Pre-fills the offset from a newly known chip family, unless the user
    /// already typed one --- never overwrites a hand-edited value.
    fn apply_default_offset(&mut self, family: ChipFamily) {
        if !self.offset_touched {
            self.offset = family.default_offset().to_string();
        }
    }

    /// Why `action` cannot run yet, if anything --- checked before it is
    /// offered a confirmation overlay and again in [`FlashPanel::run`], so a
    /// blank offset can never reach esptool as an empty positional argument
    /// (`SPEC.md` §8's "never guess" spirit: an unset value blocks the
    /// action instead of silently defaulting to some chip's convention).
    pub fn blocked_reason(&self, action: FlashAction) -> Option<&'static str> {
        if !action.needs_firmware() {
            return None;
        }
        if self.selected_firmware.is_none() {
            return Some("select a firmware file first");
        }
        if self.offset.trim().is_empty() {
            return Some("set a flash offset first (pick a chip or type one)");
        }
        None
    }

    /// The exact command an action would run, for the confirmation overlay
    /// (`SPEC.md` §15: never hide that a command is destructive behind a
    /// paraphrase) and for driving [`FlashPanel::run`] itself.
    pub fn command_preview(&self, action: FlashAction, port: Option<&str>) -> Option<String> {
        if self.blocked_reason(action).is_some() {
            return None;
        }
        self.build_command(action, port, None, false)
            .map(|command| command.to_string())
    }

    fn build_command(
        &self,
        action: FlashAction,
        port: Option<&str>,
        probe_dest: Option<&Path>,
        hunt_version: bool,
    ) -> Option<crate::process::Command> {
        let chip = self.chip.family();
        Some(match action {
            FlashAction::ChipInfo => commands::chip_id(port),
            FlashAction::FlashInfo => commands::flash_id(port),
            FlashAction::EraseFlash => commands::erase_flash(port),
            FlashAction::Reset => commands::reset(port),
            FlashAction::ReadFlash => {
                let (offset, size) = if hunt_version {
                    (firmware_id::HUNT_OFFSET, firmware_id::HUNT_SIZE)
                } else {
                    (firmware_id::READ_OFFSET, firmware_id::READ_SIZE)
                };
                commands::read_flash(port, offset, size, probe_dest?)
            }
            FlashAction::WriteFlash => commands::write_flash(
                port,
                chip,
                &self.offset,
                &self.selected_firmware_path()?,
                &self.options,
            ),
            FlashAction::VerifyFlash => {
                commands::verify_flash(port, chip, &self.offset, &self.selected_firmware_path()?)
            }
        })
    }

    /// Marks `action` as awaiting the confirmation overlay. The caller reads
    /// [`FlashPanel::command_preview`] for the message and calls
    /// [`FlashPanel::take_pending`] once the user accepts.
    pub fn request_confirmation(&mut self, action: FlashAction) {
        self.pending_action = Some(action);
    }

    pub fn take_pending(&mut self) -> Option<FlashAction> {
        self.pending_action.take()
    }

    /// The action awaiting the confirmation overlay, without consuming it:
    /// the overlay is redrawn every frame and has to name what it is
    /// asking about (`SPEC.md` §15).
    pub fn pending(&self) -> Option<FlashAction> {
        self.pending_action
    }

    pub fn cancel_pending(&mut self) {
        self.pending_action = None;
    }

    /// Starts `action`. Returns a warning instead of spawning when a
    /// firmware-dependent action has none selected, or another command is
    /// already running. On success, the caller (`App`) is responsible for
    /// moving the user's attention to the dashboard's Monitor tab, which
    /// renders [`Self::output`] as it streams --- this panel no longer owns
    /// an "output screen" of its own.
    pub fn run(
        &mut self,
        action: FlashAction,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> Vec<Notice> {
        let notices = self.spawn(action, processes, port, false, None, false);
        if notices.is_empty() {
            self.output.clear();
        }
        notices
    }

    /// Refreshes chip identity in the background --- e.g. right after
    /// a device is selected, so the Dashboard's device panel has something
    /// to show without the user ever opening the Flash view. `esptool
    /// chip-id` is the cheapest command that reads the connection banner
    /// (chip/revision/features/crystal/MAC); unlike [`Self::run`], this
    /// never touches [`Self::screen`]: it must not navigate the user away
    /// from whatever they are currently looking at (`self.output` still
    /// accumulates lines the normal way, but that is invisible until
    /// [`Self::run`] switches to the output screen, which clears it first).
    /// A command already running (including one the user started by hand)
    /// is left alone rather than queued --- there is no request queue here
    /// (see the module doc), and this is a courtesy refresh, not something
    /// worth interrupting real work for. Returns whether the query started;
    /// `false` means it was refused and nothing will follow.
    pub fn query_device_info(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> bool {
        self.spawn(FlashAction::ChipInfo, processes, port, true, None, false)
            .is_empty()
    }

    /// Whether the background identity query ([`Self::query_device_info`])
    /// is in flight: the first device listing is held behind it, and the
    /// firmware gate that follows only applies once it reports back ---
    /// a listing must not slip past between the query's start and its
    /// finish event.
    pub fn chip_query_running(&self) -> bool {
        self.in_flight
            .as_ref()
            .is_some_and(|running| running.background && running.action == FlashAction::ChipInfo)
    }

    /// Reads the flash region [`crate::firmware_id`] needs and identifies
    /// the installed firmware from it --- the second background query after
    /// [`Self::query_device_info`], started only once the user has answered
    /// the identification question (esptool resets the board into its
    /// bootloader to read flash, stopping whatever the firmware was doing;
    /// that consent is the caller's business). Same rules as the identity
    /// query otherwise: never navigates, and a busy panel refuses rather
    /// than queues. Returns whether the query started.
    pub fn query_firmware_identity(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> bool {
        let dest = firmware_probe_path();
        self.spawn(
            FlashAction::ReadFlash,
            processes,
            port,
            true,
            Some(dest),
            false,
        )
        .is_empty()
    }

    /// Whether the identification read left a firmware named without a
    /// version, i.e. whether [`Self::query_firmware_version`] has anything
    /// to do.
    pub fn has_pending_version_hunt(&self) -> bool {
        self.version_hunt_pending
    }

    /// Drops an armed version hunt without running it (e.g. the port it was
    /// armed for is gone).
    pub fn drop_version_hunt(&mut self) {
        self.version_hunt_pending = false;
    }

    /// The follow-up to a versionless verdict: `esptool read-flash` over the
    /// window after the identification one, hunting only the version of the
    /// firmware already named (a Zephyr *simple boot* image carries its
    /// application banner far deeper than the identification window ---
    /// see [`firmware_id::HUNT_OFFSET`]). Same rules as the other background
    /// queries: never navigates, refuses when busy, and --- being pure
    /// courtesy --- a refusal simply leaves the firmware bare. Returns
    /// whether the hunt started.
    pub fn query_firmware_version(
        &mut self,
        processes: &mut ProcessManager,
        port: Option<&str>,
    ) -> bool {
        // Only a verdict that still stands, still versionless, may be filled
        // in: a cleared or switched identity means the hunt is moot.
        if !self.version_hunt_pending
            || !matches!(
                self.details.firmware,
                Some(FirmwareVerdict::Firmware(_, None))
            )
        {
            self.version_hunt_pending = false;
            return false;
        }
        let dest = firmware_probe_path();
        let started = self
            .spawn(
                FlashAction::ReadFlash,
                processes,
                port,
                true,
                Some(dest),
                true,
            )
            .is_empty();
        if started {
            self.version_hunt_pending = false;
        }
        started
    }

    fn spawn(
        &mut self,
        action: FlashAction,
        processes: &mut ProcessManager,
        port: Option<&str>,
        background: bool,
        probe_dest: Option<PathBuf>,
        hunt_version: bool,
    ) -> Vec<Notice> {
        if self.is_busy() {
            return vec![(Level::Warn, "a command is already running".to_string())];
        }
        if let Some(reason) = self.blocked_reason(action) {
            return vec![(Level::Warn, reason.to_string())];
        }
        let Some(command) = self.build_command(action, port, probe_dest.as_deref(), hunt_version)
        else {
            return vec![(Level::Warn, "cannot build that command".to_string())];
        };
        let command = match &self.tool_path {
            Some(program) => command.with_program(program),
            None => command,
        };

        let id = processes.spawn(command, FLASH_TIMEOUT);
        self.in_flight = Some(RunningCommand {
            id,
            action,
            background,
            probe_dest,
            hunt_version,
            started: Instant::now(),
            stdout: String::new(),
            stderr: String::new(),
            progress: None,
        });
        self.state = RunState::Running;
        // A user-started command puts the tab's `Stop` row in the list from
        // this moment: park the cursor on it (the build panel's rule), so
        // cancelling is one Enter away. Background queries never move the
        // cursor --- the user did not leave it.
        if !background {
            self.pane_cursor = self.pane_actions().len() - 1;
        }
        Vec::new()
    }

    /// Feeds a process event back into the panel.
    pub fn on_process(&mut self, event: &ProcessEvent) -> FlashUpdate {
        let mut update = FlashUpdate::default();

        match event {
            ProcessEvent::Started { .. } => return update,
            ProcessEvent::Output { .. } => return update,
            ProcessEvent::Line { id, stream, text } => {
                if let Some(running) = &mut self.in_flight
                    && running.id == *id
                {
                    let buffer = match stream {
                        Stream::Stdout => &mut running.stdout,
                        Stream::Stderr => &mut running.stderr,
                    };
                    buffer.push_str(text);
                    buffer.push('\n');
                    if let Some(progress) = crate::progress::detect(text) {
                        running.progress = Some(progress);
                    }
                    self.output.push(text.clone());
                }
                return update;
            }
            ProcessEvent::Finished {
                id,
                outcome,
                duration,
            } => {
                let Some(running) = self.in_flight.take() else {
                    return update;
                };
                if running.id != *id {
                    // Not ours (another subsystem's process): put it back.
                    self.in_flight = Some(running);
                    return update;
                }
                self.complete(running, outcome, *duration, &mut update);
            }
        }

        update
    }

    fn complete(
        &mut self,
        running: RunningCommand,
        outcome: &Outcome,
        duration: Duration,
        update: &mut FlashUpdate,
    ) {
        let failure = match outcome {
            Outcome::Success => None,
            Outcome::SpawnFailed(_) => Some(format!(
                "{} is not on PATH — install it to flash firmware",
                commands::PROGRAM
            )),
            Outcome::TimedOut => Some(format!(
                "{} did not respond within {}s",
                commands::PROGRAM,
                FLASH_TIMEOUT.as_secs()
            )),
            Outcome::Cancelled => Some("cancelled".to_string()),
            Outcome::Failed { .. } => Some(parse::explain_error(&running.stderr)),
        };

        // Opportunistic: every esptool command that reaches the board prints
        // a "Chip is ESP32-..." banner, so detection never needs a dedicated
        // probe. A manual override always wins.
        if let Some(family) = parse::parse_chip_family(&running.stdout)
            && !matches!(self.chip, ChipGuess::Overridden(_))
        {
            self.chip = ChipGuess::Detected(family);
            self.apply_default_offset(family);
        }
        self.details
            .merge(parse::parse_device_details(&running.stdout));
        if let Some(dest) = &running.probe_dest {
            if running.hunt_version {
                self.apply_version_from(dest, update);
            } else {
                self.identify_firmware_from(dest, update);
            }
        }
        if running.background {
            match running.action {
                FlashAction::ChipInfo => {
                    update.background_chip_query_finished = true;
                }
                // The hunt's finish is none of the first-listing chain's
                // business: the identification read already reported, and
                // re-driving the gate would re-refuse a settled pane.
                FlashAction::ReadFlash if running.probe_dest.is_some() && !running.hunt_version => {
                    update.background_firmware_read_finished = true;
                }
                _ => {}
            }
        }

        let ok = failure.is_none();
        match failure {
            Some(error) => {
                self.state = RunState::Failed(error.clone());
                update
                    .notices
                    .push((Level::Error, format!("{}: {error}", running.action.label())));
            }
            None => {
                self.state = RunState::Succeeded;
                // The version hunt speaks for itself --- `apply_version_from`
                // (above) already pushed a "build v…" notice when it found
                // one, and pushes nothing when it did not ("a hunt that
                // finds nothing changes nothing"). A generic "Identify
                // firmware: done" here would just repeat the identification
                // read's own line under the same action label.
                if !running.hunt_version {
                    update
                        .notices
                        .push((Level::Success, format!("{}: done", running.action.label())));
                }

                if running.action == FlashAction::EraseFlash {
                    update.notices.extend(self.discover_firmware());
                    update.offer_flash = true;
                }

                // The flash contents just changed: any firmware verdict
                // the identification read produced is obsolete.
                if matches!(
                    running.action,
                    FlashAction::EraseFlash | FlashAction::WriteFlash
                ) {
                    update.firmware_invalidated = true;
                }

                if running.action == FlashAction::FlashInfo && !running.background {
                    update.notices.extend(self.discover_firmware());
                    if self.firmware.is_empty() && self.chip.family().is_some() {
                        update.search_online_for_firmware = true;
                    }
                }
            }
        }

        // The actions tab's report line and cursor: a user-started command
        // reports its outcome and lands back on its own row (the `Stop` tail
        // just left the list); a background query is invisible courtesy work
        // and only needs a cursor left pointing past the shrunken list
        // clamped back onto it.
        if !running.background {
            self.last = Some(FlashReport {
                what: running.action.label(),
                ok,
                duration,
            });
            self.set_pane_cursor_to(running.action);
        } else {
            self.pane_cursor = self.pane_cursor.min(self.pane_actions().len() - 1);
        }
    }

    /// Parses the bytes `esptool read-flash` just wrote and records what
    /// they say; the scratch file is removed either way. A failed or
    /// unrecognized read leaves [`Self::details`]'s firmware `None`, which
    /// the Device info pane renders as `undefined`.
    fn identify_firmware_from(&mut self, dest: &Path, update: &mut FlashUpdate) {
        let verdict = std::fs::read(dest)
            .ok()
            .and_then(|data| firmware_id::classify(&data));
        let _ = std::fs::remove_file(dest);
        match verdict {
            Some(FirmwareVerdict::Firmware(firmware, version)) => {
                let named = match &version {
                    Some(version) => format!("{} {version}", firmware.label()),
                    None => firmware.label().to_string(),
                };
                // A firmware the window could name but not date asks for the
                // hunt (unless it is ESP-IDF, whose only version source is
                // the descriptor the window already read).
                self.version_hunt_pending = version.is_none() && firmware != FlashFirmware::EspIdf;
                self.details.firmware = Some(FirmwareVerdict::Firmware(firmware, version));
                update
                    .notices
                    .push((Level::Success, format!("firmware on the device: {named}")));
            }
            Some(FirmwareVerdict::Erased) => {
                self.details.firmware = Some(FirmwareVerdict::Erased);
                update.notices.push((
                    Level::Warn,
                    "no firmware: the device's flash is erased".to_string(),
                ));
            }
            None => update
                .notices
                .push((Level::Info, "firmware could not be identified".to_string())),
        }
    }

    /// Parses the bytes the version hunt's `esptool read-flash` just wrote
    /// and, if they carry the named firmware's version, fills it into the
    /// standing verdict; the scratch file is removed either way. The verdict
    /// itself is not re-judged --- the hunt reads a window the
    /// identification rules were never meant to run over, and only ever
    /// dates a firmware the first window already named. A hunt that finds
    /// nothing changes nothing: the firmware stays bare, which stays honest.
    fn apply_version_from(&mut self, dest: &Path, update: &mut FlashUpdate) {
        let standing = match &self.details.firmware {
            Some(FirmwareVerdict::Firmware(kind, None)) => Some(*kind),
            _ => None,
        };
        let version = standing.and_then(|kind| {
            std::fs::read(dest)
                .ok()
                .and_then(|data| firmware_id::version(&data, kind))
        });
        let _ = std::fs::remove_file(dest);
        if let (Some(kind), Some(version)) = (standing, version) {
            self.details.firmware = Some(FirmwareVerdict::Firmware(kind, Some(version.clone())));
            update
                .notices
                .push((Level::Info, format!("{} build {version}", kind.label())));
        }
    }

    /// Whether an esptool command or a curl fetch/download is currently
    /// running --- only one of either kind at a time (`SPEC.md` §22's "one
    /// tool at a time" convention, already followed by [`crate::browser::Browser`]).
    pub fn is_busy(&self) -> bool {
        self.in_flight.is_some() || self.in_flight_fetch.is_some()
    }

    /// A configured curl is judged the same way a resolved `west` is: the
    /// file itself, with [`crate::backend::executable_at`]. Taking
    /// `is_some()` as the answer would call an unrunnable path available and
    /// fail at spawn.
    fn curl_available(&self) -> bool {
        self.curl_tool_path.as_deref().map_or_else(
            || tool_available(curl_commands::PROGRAM),
            |path| crate::backend::executable_at(std::path::Path::new(path)),
        )
    }

    fn build_curl(&self, command: crate::process::Command) -> crate::process::Command {
        match &self.curl_tool_path {
            Some(program) => command.with_program(program),
            None => command,
        }
    }

    /// Whether a board-list search is in flight ([`Self::search_online`]).
    pub fn searching_boards(&self) -> bool {
        matches!(self.fetch_kind(), Some(FetchKind::Boards))
    }

    /// Whether a board's firmware page is being fetched
    /// ([`Self::fetch_board_page`]).
    pub fn fetching_firmware_list(&self) -> bool {
        matches!(self.fetch_kind(), Some(FetchKind::Firmware { .. }))
    }

    /// Whether a firmware download is in flight ([`Self::download`]).
    pub fn downloading_firmware(&self) -> bool {
        matches!(self.fetch_kind(), Some(FetchKind::Download { .. }))
    }

    fn fetch_kind(&self) -> Option<&FetchKind> {
        self.in_flight_fetch.as_ref().map(|fetch| &fetch.kind)
    }

    /// Searches micropython.org/download/ for boards matching `mcu` and,
    /// optionally, `vendor` (`SPEC.md` §9).
    ///
    /// Moves to [`FlashScreen::OnlineBoards`] right away so the search is
    /// visible as a window --- its own source line and a searching status ---
    /// rather than a silent wait on the menu for results that may never come.
    pub fn search_online(
        &mut self,
        mcu: &str,
        vendor: Option<&str>,
        processes: &mut ProcessManager,
    ) -> Vec<Notice> {
        if self.is_busy() {
            return vec![(Level::Warn, "a command is already running".to_string())];
        }
        if !self.curl_available() {
            return vec![(
                Level::Warn,
                "curl is not on PATH --- install it to search for firmware online".to_string(),
            )];
        }

        let url = firmware::board_list_url(mcu, vendor);
        let command = self.build_curl(curl_commands::fetch_page(&url));
        let id = processes.spawn(command, FETCH_TIMEOUT);
        self.in_flight_fetch = Some(RunningFetch {
            id,
            kind: FetchKind::Boards,
            stdout: String::new(),
        });
        self.online_boards.clear();
        self.online_source = Some(url);
        self.screen = FlashScreen::OnlineBoards;
        Vec::new()
    }

    /// Fetches `board_id`'s firmware page, listing its downloadable files.
    /// Like [`Self::search_online`], moves to the results screen
    /// ([`FlashScreen::OnlineFirmware`]) immediately so the wait is visible
    /// on the window that will show the answer.
    pub fn fetch_board_page(
        &mut self,
        board_id: &str,
        processes: &mut ProcessManager,
    ) -> Vec<Notice> {
        if self.is_busy() {
            return vec![(Level::Warn, "a command is already running".to_string())];
        }
        if !self.curl_available() {
            return vec![(
                Level::Warn,
                "curl is not on PATH --- install it to search for firmware online".to_string(),
            )];
        }

        let url = firmware::board_page_url(board_id);
        let command = self.build_curl(curl_commands::fetch_page(&url));
        let id = processes.spawn(command, FETCH_TIMEOUT);
        self.in_flight_fetch = Some(RunningFetch {
            id,
            kind: FetchKind::Firmware {
                board_id: board_id.to_string(),
            },
            stdout: String::new(),
        });
        self.online_firmware.clear();
        self.online_source = Some(url);
        self.screen = FlashScreen::OnlineFirmware;
        Vec::new()
    }

    /// Where a download of `url` would land --- `firmware_dir` plus the
    /// URL's last path segment. `None` when the URL has no usable filename.
    pub fn download_destination(&self, url: &str) -> Option<PathBuf> {
        let name = url.rsplit('/').next()?;
        if name.is_empty() {
            return None;
        }
        Some(self.firmware_dir.join(name))
    }

    /// Downloads `url` to `dest`, streamed off the UI thread via `curl`.
    /// Overwrite confirmation, if needed, is the caller's job (`App` shows
    /// [`crate::app::Overlay::ConfirmDownloadOverwrite`] before calling this)
    /// --- this method just runs, the same division of responsibility as
    /// [`FlashPanel::run`] vs. [`FlashPanel::request_confirmation`].
    pub fn download(
        &mut self,
        url: &str,
        dest: PathBuf,
        processes: &mut ProcessManager,
    ) -> Vec<Notice> {
        if self.is_busy() {
            return vec![(Level::Warn, "a command is already running".to_string())];
        }
        if !self.curl_available() {
            return vec![(
                Level::Warn,
                "curl is not on PATH --- install it to download firmware".to_string(),
            )];
        }

        let command = self.build_curl(curl_commands::download_file(url, &dest));
        let id = processes.spawn(command, DOWNLOAD_TIMEOUT);
        self.in_flight_fetch = Some(RunningFetch {
            id,
            kind: FetchKind::Download { dest },
            stdout: String::new(),
        });
        Vec::new()
    }

    pub fn move_online_cursor(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.online_cursor = 0;
            return;
        }
        self.online_cursor =
            (self.online_cursor as isize + delta).rem_euclid(len as isize) as usize;
    }

    pub fn push_custom_url_char(&mut self, c: char) {
        self.custom_url.push(c);
    }

    pub fn backspace_custom_url(&mut self) {
        self.custom_url.pop();
    }

    /// Feeds a process event back into the panel's curl fetch/download
    /// tracking, independent of [`FlashPanel::on_process`] (esptool). A
    /// no-op for an event neither started, matching `App::on_process`'s doc
    /// comment on the same convention.
    pub fn on_curl_process(&mut self, event: &ProcessEvent) -> FlashFetchUpdate {
        let mut update = FlashFetchUpdate::default();

        match event {
            ProcessEvent::Started { .. } => return update,
            ProcessEvent::Output { .. } => return update,
            ProcessEvent::Line { id, text, .. } => {
                if let Some(running) = &mut self.in_flight_fetch
                    && running.id == *id
                {
                    running.stdout.push_str(text);
                    running.stdout.push('\n');
                }
                return update;
            }
            ProcessEvent::Finished { id, outcome, .. } => {
                let Some(running) = self.in_flight_fetch.take() else {
                    return update;
                };
                if running.id != *id {
                    self.in_flight_fetch = Some(running);
                    return update;
                }
                self.complete_fetch(running, outcome, &mut update);
            }
        }

        update
    }

    fn complete_fetch(
        &mut self,
        running: RunningFetch,
        outcome: &Outcome,
        update: &mut FlashFetchUpdate,
    ) {
        if !outcome.is_success() {
            let reason = match outcome {
                Outcome::SpawnFailed(_) => {
                    format!("{} is not on PATH", curl_commands::PROGRAM)
                }
                Outcome::TimedOut => "curl did not respond in time".to_string(),
                Outcome::Cancelled => "cancelled".to_string(),
                Outcome::Failed { .. } => {
                    "curl reported an error --- check the URL and your connection".to_string()
                }
                Outcome::Success => unreachable!("guarded above"),
            };
            update.notices.push((Level::Error, reason));
            return;
        }

        match running.kind {
            FetchKind::Boards => {
                let boards = firmware::parse_board_list(&running.stdout);
                if boards.is_empty() {
                    update.notices.push((
                        Level::Warn,
                        "no boards found for this chip --- try pasting a direct URL ('u')"
                            .to_string(),
                    ));
                } else {
                    update.notices.push((
                        Level::Info,
                        format!("found {} board{}", boards.len(), plural(boards.len())),
                    ));
                    self.online_boards = boards;
                    self.online_cursor = 0;
                    self.screen = FlashScreen::OnlineBoards;
                }
            }
            FetchKind::Firmware { board_id } => {
                let mut files = firmware::parse_firmware_files(&running.stdout);
                // Only a full image is safe to write at a fixed offset; an
                // `.app-bin`/`.elf` is not something `write-flash` should get.
                files.retain(|file| file.kind == FirmwareKind::Bin);
                if files.is_empty() {
                    update.notices.push((
                        Level::Warn,
                        format!(
                            "no flashable .bin firmware found for {board_id} --- try pasting a direct URL ('u')"
                        ),
                    ));
                } else {
                    update.notices.push((
                        Level::Info,
                        format!(
                            "found {} firmware build{} for {board_id}",
                            files.len(),
                            plural(files.len())
                        ),
                    ));
                    self.online_firmware = files;
                    self.online_cursor = 0;
                    self.screen = FlashScreen::OnlineFirmware;
                }
            }
            FetchKind::Download { dest } => {
                let message = match curl_parse::parse_download_summary(&running.stdout) {
                    Some(summary) => format!(
                        "downloaded {} ({} bytes, HTTP {})",
                        dest.display(),
                        summary.bytes,
                        summary.http_code
                    ),
                    None => format!("downloaded {}", dest.display()),
                };
                update.notices.push((Level::Success, message));
                update.download_finished = true;
            }
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Cycles through `all`, wrapping in either direction. `current` outside the
/// slice (or absent) starts from an end, so a first press always lands on a
/// real value rather than requiring two.
fn cycle<T: Copy + PartialEq>(all: &[T], current: Option<T>, forward: bool) -> T {
    let index = current.and_then(|value| all.iter().position(|item| *item == value));
    let next = match (index, forward) {
        (Some(i), true) => (i + 1) % all.len(),
        (Some(i), false) => (i + all.len() - 1) % all.len(),
        (None, true) => 0,
        (None, false) => all.len() - 1,
    };
    all[next]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("chiptui-flash-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, name: &str) {
            std::fs::write(self.root.join(name), b"x").unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fake_esptool() -> String {
        format!("{}/tests/fixtures/bin/esptool", env!("CARGO_MANIFEST_DIR"))
    }

    fn fake_curl() -> String {
        format!("{}/tests/fixtures/bin/curl", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn a_single_firmware_file_selects_itself() {
        let fixture = Fixture::new("single");
        fixture.write("app.bin");
        let mut panel = FlashPanel::new(&fixture.root);

        panel.discover_firmware();
        assert_eq!(panel.selected_firmware, Some(0));
    }

    #[test]
    fn several_firmware_files_require_a_choice() {
        let fixture = Fixture::new("many");
        fixture.write("a.bin");
        fixture.write("b.elf");
        let mut panel = FlashPanel::new(&fixture.root);

        panel.discover_firmware();
        assert_eq!(panel.selected_firmware, None, "must not guess which image");
        assert_eq!(panel.firmware.len(), 2);

        assert!(panel.select_firmware(1));
        assert_eq!(panel.selected_firmware, Some(1));
    }

    #[test]
    fn no_firmware_files_leaves_the_selection_empty() {
        let fixture = Fixture::new("none");
        let mut panel = FlashPanel::new(&fixture.root);

        let notices = panel.discover_firmware();
        assert!(panel.firmware.is_empty());
        assert!(notices.iter().any(|(_, m)| m.contains("no .bin/.elf")));
    }

    #[test]
    fn only_write_and_erase_need_confirmation() {
        assert!(FlashAction::EraseFlash.is_destructive());
        assert!(FlashAction::WriteFlash.is_destructive());
        assert!(!FlashAction::VerifyFlash.is_destructive());
        assert!(!FlashAction::ChipInfo.is_destructive());
        assert!(!FlashAction::FlashInfo.is_destructive());
        assert!(!FlashAction::Reset.is_destructive());
    }

    #[test]
    fn running_without_firmware_selected_is_refused() {
        let fixture = Fixture::new("refuse");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        let notices = panel.run(FlashAction::WriteFlash, &mut processes, None);
        assert!(!panel.is_busy());
        assert!(notices.iter().any(|(_, m)| m.contains("select a firmware")));
    }

    #[test]
    fn running_with_an_empty_offset_is_refused_instead_of_sending_a_blank_argument() {
        // A firmware file alone is not enough: without a chip pick or a typed
        // offset, `esptool write-flash "" file.bin` would be sent with an
        // empty positional argument. The panel must refuse, not guess.
        let fixture = Fixture::new("no-offset");
        fixture.write("app.bin");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        panel.discover_firmware();
        assert_eq!(panel.selected_firmware, Some(0));
        assert!(panel.offset.is_empty(), "no chip has been picked yet");

        assert_eq!(
            panel.blocked_reason(FlashAction::WriteFlash),
            Some("set a flash offset first (pick a chip or type one)")
        );
        assert_eq!(panel.command_preview(FlashAction::WriteFlash, None), None);

        let mut processes = ProcessManager::new();
        let notices = panel.run(FlashAction::WriteFlash, &mut processes, None);
        assert!(!panel.is_busy(), "nothing should have been spawned");
        assert!(
            notices
                .iter()
                .any(|(_, m)| m.contains("set a flash offset"))
        );

        // Picking a chip fills the default offset, which unblocks the action.
        panel.cycle_chip(true);
        assert_eq!(panel.blocked_reason(FlashAction::WriteFlash), None);
    }

    #[test]
    fn the_offset_defaults_from_a_detected_chip_but_not_after_editing() {
        let fixture = Fixture::new("offset");
        let mut panel = FlashPanel::new(&fixture.root);

        panel.cycle_chip(true);
        assert_eq!(panel.chip.family(), Some(ChipFamily::ALL[0]));
        assert_eq!(panel.offset, ChipFamily::ALL[0].default_offset());

        panel.set_offset("0x9999".to_string());
        panel.cycle_chip(true);
        assert_eq!(
            panel.offset, "0x9999",
            "a hand-edited offset must survive a later chip change"
        );
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let mut panel = FlashPanel::new(std::env::temp_dir());
        panel.cycle_chip(false);
        assert_eq!(
            panel.chip.family(),
            Some(ChipFamily::ALL[ChipFamily::ALL.len() - 1])
        );
        panel.cycle_chip(true);
        assert_eq!(panel.chip.family(), Some(ChipFamily::ALL[0]));
    }

    #[test]
    fn erase_flash_end_to_end_offers_flash_when_firmware_is_present() {
        let fixture = Fixture::new("erase");
        fixture.write("app.bin");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        panel.run(FlashAction::EraseFlash, &mut processes, None);
        let update = settle(&mut panel, &mut processes);

        assert!(matches!(panel.state, RunState::Succeeded));
        assert!(update.offer_flash);
        assert_eq!(panel.selected_firmware, Some(0));
    }

    #[test]
    fn write_flash_succeeds_and_detects_the_chip() {
        let fixture = Fixture::new("write");
        fixture.write("app.bin");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        panel.discover_firmware();
        panel.set_offset("0x1000".to_string());
        let mut processes = ProcessManager::new();

        panel.run(FlashAction::WriteFlash, &mut processes, None);
        settle(&mut panel, &mut processes);

        assert!(matches!(panel.state, RunState::Succeeded));
        assert_eq!(panel.chip.family(), Some(ChipFamily::Esp32));
        assert!(
            panel.output.iter().any(|line| line.contains("Wrote")),
            "output lines: {:?}",
            panel.output
        );
    }

    #[test]
    fn chip_and_flash_info_accumulate_into_device_details() {
        let fixture = Fixture::new("device-details");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        panel.run(FlashAction::ChipInfo, &mut processes, None);
        settle(&mut panel, &mut processes);
        assert_eq!(panel.details.family, Some(ChipFamily::Esp32));
        assert_eq!(panel.details.mac.as_deref(), Some("24:6f:28:12:34:56"));
        assert_eq!(
            panel.details.flash_size, None,
            "chip-id never mentions flash"
        );

        panel.run(FlashAction::FlashInfo, &mut processes, None);
        let update = settle(&mut panel, &mut processes);
        assert_eq!(panel.details.flash_size.as_deref(), Some("4MB"));
        assert_eq!(
            panel.details.mac.as_deref(),
            Some("24:6f:28:12:34:56"),
            "the earlier chip-id run's MAC must survive a flash-id run that repeats it"
        );
        assert!(
            update.search_online_for_firmware,
            "a user-initiated flash-info with an empty firmware/ dir and a known \
             chip should offer an online search"
        );
    }

    #[test]
    fn query_device_info_populates_details_without_touching_the_screen() {
        // The background refresh must not hijack whatever the user is
        // looking at: unlike `run`, it never navigates to the output screen.
        let fixture = Fixture::new("query-device-info");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(
            panel.query_device_info(&mut processes, None),
            "with the panel idle the query must start"
        );
        assert!(panel.is_busy());
        assert_eq!(panel.screen, FlashScreen::Menu, "screen must stay put");

        let update = settle(&mut panel, &mut processes);

        assert_eq!(panel.details.family, Some(ChipFamily::Esp32));
        assert_eq!(panel.details.mac.as_deref(), Some("24:6f:28:12:34:56"));
        assert_eq!(
            panel.details.flash_size, None,
            "the background query is chip-id, which never mentions flash"
        );
        assert!(
            update.background_chip_query_finished,
            "whatever was chained behind the query may proceed"
        );
        assert_eq!(
            panel.screen,
            FlashScreen::Menu,
            "a silent refresh must not navigate to the output screen"
        );
        assert!(
            !update.search_online_for_firmware,
            "a courtesy background refresh must never trigger an unprompted \
             online firmware search, even with an empty firmware/ dir and a \
             known chip"
        );
    }

    #[test]
    fn query_device_info_is_a_silent_no_op_while_something_else_is_running() {
        let fixture = Fixture::new("query-device-info-busy");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        panel.run(FlashAction::ChipInfo, &mut processes, None);
        assert!(panel.is_busy());

        // Must not queue, replace the in-flight command, or panic.
        assert!(!panel.query_device_info(&mut processes, None));
        settle(&mut panel, &mut processes);
        assert_eq!(panel.details.family, Some(ChipFamily::Esp32));
    }

    #[test]
    fn query_firmware_identity_identifies_from_the_read() {
        use crate::firmware_id::FlashFirmware;

        let fixture = Fixture::new("query-firmware-identity");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(
            panel.query_firmware_identity(&mut processes, None),
            "with the panel idle the probe must start"
        );
        let update = settle(&mut panel, &mut processes);

        assert_eq!(
            panel.details.firmware,
            Some(FirmwareVerdict::Firmware(
                FlashFirmware::MicroPython,
                Some("v1.28.0".to_string())
            ))
        );
        assert_eq!(
            panel.screen,
            FlashScreen::Menu,
            "a background query must not navigate anywhere"
        );
        assert!(
            update
                .notices
                .iter()
                .any(|(level, message)| matches!(level, Level::Success)
                    && message.contains("MicroPython v1.28.0")),
            "the identification result and its version must reach the log: {:?}",
            update.notices
        );
    }

    #[test]
    fn query_firmware_identity_reads_a_zephyr_board() {
        use crate::firmware_id::FlashFirmware;

        let fixture = Fixture::new("query-firmware-identity-zephyr");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(panel.query_firmware_identity(&mut processes, Some("/dev/ttyUSB1")));
        settle(&mut panel, &mut processes);

        assert_eq!(
            panel.details.firmware,
            Some(FirmwareVerdict::Firmware(
                FlashFirmware::Zephyr,
                Some("v4.0.0".to_string())
            ))
        );
        assert!(
            !panel.has_pending_version_hunt(),
            "a banner the window itself dated needs no hunt"
        );
    }

    #[test]
    fn a_versionless_zephyr_verdict_is_dated_by_the_follow_up_hunt() {
        use crate::firmware_id::FlashFirmware;

        let fixture = Fixture::new("query-firmware-version-hunt");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        // A simple-boot board: the identification window names Zephyr
        // through the kernel's strings, but the banner --- and the version
        // in it --- sits deep past it.
        assert!(panel.query_firmware_identity(&mut processes, Some("/dev/ttyUSB4")));
        settle(&mut panel, &mut processes);
        assert_eq!(
            panel.details.firmware,
            Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None))
        );
        assert!(
            panel.has_pending_version_hunt(),
            "a firmware named without a version arms the hunt"
        );

        // The hunt reads the follow-up window and dates the standing
        // verdict; the identification is not re-judged.
        assert!(panel.query_firmware_version(&mut processes, Some("/dev/ttyUSB4")));
        let update = settle(&mut panel, &mut processes);
        assert_eq!(
            panel.details.firmware,
            Some(FirmwareVerdict::Firmware(
                FlashFirmware::Zephyr,
                Some("v4.4.0-11847-gc5dffcb7c9da".to_string())
            ))
        );
        assert!(!panel.has_pending_version_hunt());
        assert!(
            update
                .notices
                .iter()
                .any(|(level, message)| matches!(level, Level::Info)
                    && message.contains("Zephyr build v4.4.0")),
            "the hunt's answer must reach the log: {:?}",
            update.notices
        );
        assert!(
            !update
                .notices
                .iter()
                .any(|(_, message)| message == "Identify firmware: done"),
            "the hunt's own \"Zephyr build …\" notice already reports its \
             completion --- a generic \"Identify firmware: done\" would just \
             repeat the identification read's line under the same label: {:?}",
            update.notices
        );
    }

    #[test]
    fn a_hunt_that_finds_nothing_leaves_the_verdict_bare() {
        use crate::firmware_id::FlashFirmware;

        let fixture = Fixture::new("query-firmware-version-hunt-empty");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(panel.query_firmware_identity(&mut processes, Some("/dev/ttyUSB4")));
        settle(&mut panel, &mut processes);

        // The hunt runs against a window with no Zephyr banner (here: a
        // MicroPython board's): the verdict stays exactly as it was.
        assert!(panel.query_firmware_version(&mut processes, Some("/dev/ttyUSB0")));
        let update = settle(&mut panel, &mut processes);
        assert_eq!(
            panel.details.firmware,
            Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None)),
            "a failed hunt must not re-judge the firmware or invent a version"
        );
        assert!(!panel.has_pending_version_hunt());
        assert!(
            update.notices.is_empty(),
            "a hunt that changes nothing must say nothing --- not even a \
             generic \"Identify firmware: done\": {:?}",
            update.notices
        );
    }

    #[test]
    fn a_cleared_identity_disarms_the_hunt() {
        let fixture = Fixture::new("query-firmware-version-hunt-cleared");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(panel.query_firmware_identity(&mut processes, Some("/dev/ttyUSB4")));
        settle(&mut panel, &mut processes);
        assert!(panel.has_pending_version_hunt());

        // The board went away: whatever the hunt might still read belongs
        // to no standing verdict.
        panel.clear_firmware_identity();
        assert!(!panel.has_pending_version_hunt());
        assert!(!panel.query_firmware_version(&mut processes, Some("/dev/ttyUSB4")));
        settle(&mut panel, &mut processes);
        assert_eq!(panel.details.firmware, None);
    }

    #[test]
    fn a_versioned_micropython_verdict_never_hunts() {
        use crate::firmware_id::FlashFirmware;

        let fixture = Fixture::new("query-firmware-version-hunt-mpy");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(panel.query_firmware_identity(&mut processes, None));
        settle(&mut panel, &mut processes);
        assert_eq!(
            panel.details.firmware,
            Some(FirmwareVerdict::Firmware(
                FlashFirmware::MicroPython,
                Some("v1.28.0".to_string())
            ))
        );
        assert!(!panel.has_pending_version_hunt());
        assert!(
            !panel.query_firmware_version(&mut processes, None),
            "nothing is armed: the hunt must refuse to run"
        );
    }

    #[test]
    fn query_firmware_identity_reads_a_blank_board() {
        let fixture = Fixture::new("query-firmware-identity-blank");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(panel.query_firmware_identity(&mut processes, Some("/dev/ttyUSB3")));
        let update = settle(&mut panel, &mut processes);

        assert_eq!(
            panel.details.firmware,
            Some(FirmwareVerdict::Erased),
            "an all-0xFF read is a chip with no firmware, not an unrecognized one"
        );
        assert!(
            update
                .notices
                .iter()
                .any(|(level, message)| matches!(level, Level::Warn) && message.contains("erased")),
            "a blank device must be named as blank: {:?}",
            update.notices
        );
    }

    #[test]
    fn query_firmware_identity_reads_a_plain_espidf_board() {
        use crate::firmware_id::FlashFirmware;

        let fixture = Fixture::new("query-firmware-identity-espidf");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(panel.query_firmware_identity(&mut processes, Some("/dev/ttyUSB2")));
        settle(&mut panel, &mut processes);

        assert_eq!(
            panel.details.firmware,
            Some(FirmwareVerdict::Firmware(
                FlashFirmware::EspIdf,
                Some("v5.3.1".to_string())
            )),
            "the esp_app_desc magic names a plain IDF app, and the descriptor's stamp names its version"
        );
    }

    #[test]
    fn query_firmware_identity_refuses_while_something_else_is_running() {
        let fixture = Fixture::new("query-firmware-identity-busy");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        panel.run(FlashAction::ChipInfo, &mut processes, None);
        assert!(panel.is_busy());

        assert!(!panel.query_firmware_identity(&mut processes, None));
        settle(&mut panel, &mut processes);
        assert_eq!(
            panel.details.firmware, None,
            "a refused probe must not leave an answer behind"
        );
    }

    #[test]
    fn write_flash_reports_a_missing_file() {
        let fixture = Fixture::new("missing");
        fixture.write("missing.bin");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        panel.discover_firmware();
        panel.set_offset("0x0".to_string());
        let mut processes = ProcessManager::new();

        panel.run(FlashAction::WriteFlash, &mut processes, None);
        settle(&mut panel, &mut processes);

        match &panel.state {
            RunState::Failed(error) => assert!(error.contains("could not be found")),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// Drives the panel until no command is outstanding, returning the last
    /// [`FlashUpdate`] produced.
    fn settle(panel: &mut FlashPanel, processes: &mut ProcessManager) -> FlashUpdate {
        use std::time::Instant;

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last = FlashUpdate::default();

        while panel.is_busy() && Instant::now() < deadline {
            for event in processes.drain() {
                let update = panel.on_process(&event);
                if !update.notices.is_empty()
                    || update.offer_flash
                    || update.search_online_for_firmware
                {
                    last = update;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(!panel.is_busy(), "command never completed");
        last
    }

    /// Same as [`settle`], but drains through [`FlashPanel::on_curl_process`].
    fn settle_fetch(panel: &mut FlashPanel, processes: &mut ProcessManager) -> FlashFetchUpdate {
        use std::time::Instant;

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last = FlashFetchUpdate::default();

        while panel.is_busy() && Instant::now() < deadline {
            for event in processes.drain() {
                let update = panel.on_curl_process(&event);
                if !update.notices.is_empty() || update.download_finished {
                    last = update;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(!panel.is_busy(), "curl command never completed");
        last
    }

    #[test]
    fn download_destination_uses_the_urls_last_path_segment() {
        let panel = FlashPanel::new("/project");
        assert_eq!(
            panel.download_destination("https://micropython.org/resources/firmware/app.bin"),
            Some(PathBuf::from("/project/app.bin"))
        );
        assert_eq!(
            panel.download_destination("https://micropython.org/resources/firmware/"),
            None,
            "no filename to use"
        );
    }

    #[test]
    fn searching_online_finds_every_board_for_an_mcu_only_query() {
        let fixture = Fixture::new("search-multi");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        panel.search_online("esp32", None, &mut processes);
        settle_fetch(&mut panel, &mut processes);

        assert_eq!(panel.screen, FlashScreen::OnlineBoards);
        assert_eq!(panel.online_boards.len(), 2);
    }

    #[test]
    fn searching_online_narrows_by_vendor() {
        let fixture = Fixture::new("search-vendor");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        panel.search_online("esp32", Some("Espressif"), &mut processes);
        settle_fetch(&mut panel, &mut processes);

        assert_eq!(panel.online_boards.len(), 1);
        assert_eq!(panel.online_boards[0].id, "ESP32_GENERIC");
    }

    #[test]
    fn searching_online_moves_to_the_boards_window_immediately() {
        let fixture = Fixture::new("search-immediate");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        let notices = panel.search_online("esp32", None, &mut processes);
        assert!(
            notices.is_empty(),
            "a started search has nothing to say yet"
        );
        assert!(panel.is_busy());
        assert_eq!(
            panel.screen,
            FlashScreen::OnlineBoards,
            "the search must be visible as a window while it runs"
        );
        assert!(panel.searching_boards());
        assert_eq!(
            panel.online_source.as_deref(),
            Some("https://micropython.org/download/?mcu=esp32"),
            "the window must be able to name where the results come from"
        );

        settle_fetch(&mut panel, &mut processes);
        assert_eq!(panel.online_boards.len(), 2);
        assert!(!panel.searching_boards());
    }

    #[test]
    fn a_fetch_is_named_and_stoppable_like_any_other_command() {
        // `is_busy` is what puts `Stop` on the actions tab, and a fetch
        // makes it busy: the row has to reach the fetch, and the state
        // line has to say what the dimmed buttons are waiting for.
        let fixture = Fixture::new("fetch-stop");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        panel.search_online("esp32c3", None, &mut processes);
        assert_eq!(panel.activity(), Some(Activity::Search));
        assert!(
            matches!(panel.pane_actions().last(), Some(FlashPaneAction::Stop)),
            "the tab offers a Stop for it"
        );

        assert!(panel.stop(&mut processes), "and the Stop reaches it");
        settle_fetch(&mut panel, &mut processes);
        assert_eq!(panel.activity(), None);
        assert!(
            panel.last.is_none(),
            "a fetch is not one of the user's commands to report"
        );
    }

    #[test]
    fn a_background_query_is_named_but_never_reported() {
        // The courtesy identity query dims every button while it holds the
        // port, so the pane names it --- without counting it as the user's
        // work or leaving a result line behind.
        let fixture = Fixture::new("query-activity");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        let mut processes = ProcessManager::new();

        assert!(panel.query_device_info(&mut processes, Some("/dev/ttyUSB0")));
        assert_eq!(panel.activity(), Some(Activity::Query));

        settle(&mut panel, &mut processes);
        assert_eq!(panel.activity(), None);
        assert!(panel.last.is_none(), "background work reports nothing");
    }

    #[test]
    fn a_refused_search_leaves_the_screen_alone() {
        // A curl path that exists but is not executable: the search cannot
        // start (deterministic on any machine), so it must not navigate
        // anywhere either.
        let fixture = Fixture::new("search-refused");
        std::fs::write(fixture.root.join("not-curl"), b"x").unwrap();
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fixture.root.join("not-curl").display().to_string());
        let mut processes = ProcessManager::new();

        let notices = panel.search_online("esp32", None, &mut processes);
        assert_eq!(panel.screen, FlashScreen::Menu);
        assert!(panel.online_source.is_none());
        assert!(notices.iter().any(|(_, m)| m.contains("curl")));
    }

    #[test]
    fn searching_online_with_no_matches_keeps_the_window_with_the_reason() {
        let fixture = Fixture::new("search-empty");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        panel.search_online("esp32c3", None, &mut processes);
        let update = settle_fetch(&mut panel, &mut processes);

        assert_eq!(
            panel.screen,
            FlashScreen::OnlineBoards,
            "the window stays open showing the no-match reason, not the menu"
        );
        assert!(panel.online_boards.is_empty());
        assert!(
            update
                .notices
                .iter()
                .any(|(_, m)| m.contains("no boards found"))
        );
    }

    #[test]
    fn fetching_a_board_page_moves_to_the_firmware_window_immediately() {
        let fixture = Fixture::new("board-page-immediate");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        panel.fetch_board_page("ESP32_GENERIC", &mut processes);
        assert!(panel.is_busy());
        assert_eq!(panel.screen, FlashScreen::OnlineFirmware);
        assert!(panel.fetching_firmware_list());
        assert_eq!(
            panel.online_source.as_deref(),
            Some("https://micropython.org/download/ESP32_GENERIC/")
        );

        settle_fetch(&mut panel, &mut processes);
        assert!(!panel.fetching_firmware_list());
    }

    #[test]
    fn fetching_a_board_page_only_offers_flashable_bin_files() {
        let fixture = Fixture::new("board-page");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        panel.fetch_board_page("ESP32_GENERIC", &mut processes);
        settle_fetch(&mut panel, &mut processes);

        assert_eq!(panel.screen, FlashScreen::OnlineFirmware);
        assert_eq!(
            panel.online_firmware.len(),
            1,
            "the .app-bin link must not be offered as a candidate: {:?}",
            panel.online_firmware
        );
        assert_eq!(panel.online_firmware[0].kind, FirmwareKind::Bin);
        assert_eq!(panel.online_firmware[0].version, "v1.28.0");
    }

    #[test]
    fn download_writes_the_file_and_reports_success() {
        let fixture = Fixture::new("download-ok");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        let url = "https://micropython.org/resources/firmware/ESP32_GENERIC-20260406-v1.28.0.bin";
        let dest = panel.download_destination(url).unwrap();
        panel.download(url, dest.clone(), &mut processes);
        let update = settle_fetch(&mut panel, &mut processes);

        assert!(update.download_finished);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "firmware-bytes");
        assert!(
            update
                .notices
                .iter()
                .any(|(level, m)| *level == Level::Success && m.contains("200"))
        );
    }

    #[test]
    fn download_reports_an_http_failure() {
        let fixture = Fixture::new("download-404");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        let url = "https://micropython.org/resources/firmware/missing.bin";
        let dest = panel.download_destination(url).unwrap();
        panel.download(url, dest, &mut processes);
        let update = settle_fetch(&mut panel, &mut processes);

        assert!(!update.download_finished);
        assert!(
            update
                .notices
                .iter()
                .any(|(level, _)| *level == Level::Error)
        );
    }

    #[test]
    fn an_esptool_action_and_an_online_fetch_cannot_run_at_the_same_time() {
        let fixture = Fixture::new("mutual-exclusion");
        let mut panel = FlashPanel::new(&fixture.root);
        panel.set_tool_path(fake_esptool());
        panel.set_curl_tool_path(fake_curl());
        let mut processes = ProcessManager::new();

        panel.search_online("esp32", None, &mut processes);
        assert!(panel.is_busy());

        let notices = panel.run(FlashAction::ChipInfo, &mut processes, None);
        assert!(
            notices.iter().any(|(_, m)| m.contains("already running")),
            "an esptool action must not start while a fetch is in flight"
        );

        settle_fetch(&mut panel, &mut processes);
    }
}
