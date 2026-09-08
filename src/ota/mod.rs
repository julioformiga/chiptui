//! How a built image reaches a running board without a cable.
//!
//! A sibling of [`crate::install`] rather than a child of
//! [`crate::backend::zephyr`], and deliberately: `backend` answers *which
//! framework is this project*, and this module answers *how does an image
//! get there*. The two are independent --- the same MCUmgr exchange serves
//! any Zephyr board with MCUboot, and a future HTTP mechanism would serve
//! the same projects differently --- so neither belongs inside the other.
//!
//! This module carries the vocabulary, the [`OtaMethodDriver`] trait and the
//! context a driver is asked with. The drivers that turn a method into
//! commands live beside it ([`mcumgr`]), and the flows that run them arrive
//! later.

pub mod address;
pub mod mcumgr;
pub mod prepare;
pub mod registry;
pub mod update;

use std::path::Path;
use std::time::Duration;

use crate::process::Command;
use crate::progress::Progress;

/// The over-the-air mechanisms ChipTUI knows how to drive.
///
/// Adding a variant is a deliberate act. Nothing in the UI matches on it to
/// decide what to offer --- that is [`crate::backend::Capability`]'s job for
/// backends and a driver's job here --- so a new mechanism does not scatter
/// `match` arms through the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OtaMethod {
    /// MCUmgr over SMP, driven by the `smpmgr` client. Zephyr's own device
    /// management protocol, and the default because it is the one a Zephyr
    /// project gets by enabling Kconfig symbols rather than by writing code.
    #[default]
    Mcumgr,
}

impl OtaMethod {
    pub const ALL: &'static [OtaMethod] = &[OtaMethod::Mcumgr];

    /// The spelling in `chiptui.toml`. Stable: it is written into projects.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Mcumgr => "mcumgr",
        }
    }

    /// The spelling shown to a person.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mcumgr => "MCUmgr (SMP)",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|method| method.id() == id)
    }

    /// The client executables this mechanism delegates to.
    ///
    /// Reported as missing, never installed --- `install::prereq`'s rule,
    /// and the same one [`crate::backend::Backend::required_tools`] follows.
    pub const fn required_tools(self) -> &'static [&'static str] {
        match self {
            Self::Mcumgr => &["smpmgr"],
        }
    }

    /// The transports this mechanism can carry.
    pub const fn transports(self) -> &'static [Transport] {
        match self {
            Self::Mcumgr => &[Transport::Udp, Transport::Serial, Transport::Ble],
        }
    }
}

/// How the client reaches the board.
///
/// Per-mechanism in principle; in practice MCUmgr's three, because SMP is
/// one protocol over three carriers and the client picks with a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    /// Over the network, to the address the board got from DHCP. The board
    /// must be on a reachable network and carrying the UDP transport.
    #[default]
    Udp,
    /// Over the same USB or UART line the console uses.
    Serial,
    /// Over Bluetooth Low Energy.
    Ble,
}

impl Transport {
    pub const ALL: &'static [Transport] = &[Transport::Udp, Transport::Serial, Transport::Ble];

    /// The spelling in `chiptui.toml`.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Serial => "serial",
            Self::Ble => "ble",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Udp => "UDP",
            Self::Serial => "serial",
            Self::Ble => "BLE",
        }
    }

    /// What the address in [`OtaConfig::address`] means for this transport,
    /// so a prompt can say what it is asking for.
    pub const fn address_label(self) -> &'static str {
        match self {
            Self::Udp => "IP address",
            Self::Serial => "serial port",
            Self::Ble => "Bluetooth address",
        }
    }

    /// The Kconfig symbol that actually carries this transport, and the
    /// dependencies Zephyr requires for it to survive a build.
    ///
    /// Read from the tree, not from memory
    /// (`subsys/mgmt/mcumgr/transport/Kconfig.{udp,uart,bluetooth}`): every
    /// one of these is a `depends on`, never a `select`, so a project that
    /// does not meet them gets the symbol silently dropped to `n` --- the
    /// build succeeds, the prepare reports every step done, and the board
    /// never answers. [`crate::ota::prepare::TransportCheck`] is what turns
    /// that silence into a row.
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Udp => "CONFIG_MCUMGR_TRANSPORT_UDP",
            // The symbol really is UART: smpmgr's `--port` names the
            // transport, Kconfig names the peripheral class.
            Self::Serial => "CONFIG_MCUMGR_TRANSPORT_UART",
            Self::Ble => "CONFIG_MCUMGR_TRANSPORT_BT",
        }
    }

    /// What the symbol depends on, phrased for a row that has to explain a
    /// build that dropped it.
    pub const fn requires(self) -> &'static str {
        match self {
            // Both of which need a network stack the project brings
            // itself --- said by the fragment's own header, not here: this
            // string rides a checklist row with about seventy columns.
            Self::Udp => "NET_UDP and NET_SOCKETS",
            Self::Serial => "UART_MCUMGR, BASE64, CONSOLE and CRC",
            Self::Ble => "BT_PERIPHERAL",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.id() == id)
    }
}

/// A project's `[ota]` answers.
///
/// Every field but the address has a default, so a hand-written section
/// carrying only an address is a complete answer --- the common case, since
/// the mechanism and the transport are what a Zephyr project almost always
/// wants and the address is the one fact ChipTUI cannot know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtaConfig {
    pub method: OtaMethod,
    pub transport: Transport,
    /// Where the board is, meaning whatever [`Transport::address_label`]
    /// says. `None` until someone answers.
    pub address: Option<String>,
    /// Whether a verified swap is made permanent without a second question.
    ///
    /// `true` by default: the cycle's own confirm is what authorized the
    /// update, and a board that has swapped, come back and answered with
    /// the hash the cycle armed has passed every check the tool can make
    /// --- so the runner finishes the job rather than parking on a question
    /// whose answer is already known.
    ///
    /// **What that spends is the revert.** An unconfirmed image is backed
    /// out by any reset; a confirmed one is permanent, and an image that
    /// boots and answers `smpmgr` can still be broken in ways neither
    /// notices. `auto_confirm = false` in `[ota]` restores the halt, the
    /// button becoming [`update::OtaAction::ConfirmImage`] until the user
    /// says so.
    pub auto_confirm: bool,
}

impl Default for OtaConfig {
    fn default() -> Self {
        Self {
            method: OtaMethod::default(),
            transport: Transport::default(),
            address: None,
            auto_confirm: true,
        }
    }
}

/// One step of an update cycle.
///
/// A shared union across every mechanism: the runner and the modal iterate a
/// driver's [`OtaMethodDriver::stages`], so no code outside a driver may
/// assume the MCUmgr seven exist --- a second mechanism declares its own
/// list, possibly with variants of its own added here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtaStage {
    /// Is the board listening at all.
    Probe,
    /// Push the signed image into the inactive slot.
    Upload,
    /// Read the slots back, answering the uploaded image's hash.
    ReadState,
    /// Mark the uploaded image for a test swap on the next boot.
    MarkPending,
    /// Reboot the board; the bootloader performs the swap.
    Reset,
    /// Read the slots again after the swap: the new image must be active.
    Verify,
    /// Make the running image permanent. Deliberately a stage of its own:
    /// until it runs, the next reset reverts --- the safety net the
    /// mechanism exists to provide, so the runner stops in front of it.
    Confirm,
}

impl OtaStage {
    pub const ALL: &'static [OtaStage] = &[
        Self::Probe,
        Self::Upload,
        Self::ReadState,
        Self::MarkPending,
        Self::Reset,
        Self::Verify,
        Self::Confirm,
    ];

    /// The row label a checklist renders.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Probe => "Probe",
            Self::Upload => "Upload",
            Self::ReadState => "Read state",
            Self::MarkPending => "Mark pending",
            Self::Reset => "Reset",
            Self::Verify => "Verify",
            Self::Confirm => "Confirm",
        }
    }

    /// Whether the stage changes what the board runs (`SPEC.md` §15's rule,
    /// read off the stage rather than written into every view). `Upload`
    /// writes only the *inactive* slot --- the running image is untouched
    /// and a half-written slot is rejected by the bootloader, so it is not
    /// destructive; arming the swap, performing it and making it permanent
    /// are.
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::MarkPending | Self::Reset | Self::Confirm)
    }

    /// Whether the stage puts bytes on the board at all: the destructive
    /// three plus [`Self::Upload`], which is not destructive (it writes the
    /// *inactive* slot) but is still megabytes over the wire and never
    /// something to start unasked. The reads are what is left, and a run
    /// with only those ahead of it is what
    /// [`update::OtaPanel::resume_writes`] answers `false` for.
    pub const fn writes(self) -> bool {
        self.is_destructive() || matches!(self, Self::Upload)
    }

    /// Not one blanket timeout: an upload is ~30 s over UDP and a bad link
    /// stretches that into minutes, while a state read answers in a
    /// heartbeat or not at all.
    pub const fn timeout(self) -> Duration {
        match self {
            Self::Upload => Duration::from_secs(300),
            _ => Duration::from_secs(15),
        }
    }

    /// Dead time *after* the stage: the bootloader's swap, during which the
    /// board answers nothing and there is no command to wait on --- so it is
    /// modelled as a settle rather than as a longer timeout on the reset
    /// itself, which would be waiting on a request that already returned.
    ///
    /// **90 s, and the number is measured rather than estimated.** It was
    /// 45 s, from the hand-run notes, and a full cycle against the reference
    /// board on 2026-09-07 had it answering again at **58 s** for a 1.3 MB
    /// image --- so `Verify` would have run into a board still swapping,
    /// failed, and left the button on `Retry` at the exact moment the update
    /// had in fact worked. A swap moves both images, so the real figure
    /// scales with size and 58 s is a floor, not a ceiling: the constant
    /// carries headroom over the measurement instead of matching it.
    ///
    /// Overshooting costs a countdown the user watches
    /// (`OtaPanel::settling_remaining` renders it) on a board that is
    /// already back. Undershooting costs a false failure on a successful
    /// update, which is the worse of the two --- and the only one that
    /// misreports what happened. So this is a **ceiling**, not a schedule:
    /// the runner polls the board through it
    /// (`update::SETTLE_POLL_INTERVAL`) and moves on the moment slot 0
    /// reports the image the cycle armed, which is what keeps the headroom
    /// from being spent on every board that does not need it.
    pub const fn settle(self) -> Duration {
        match self {
            Self::Reset => Duration::from_secs(90),
            _ => Duration::ZERO,
        }
    }
}

/// What a driver is handed to build a stage's command.
pub struct OtaContext<'a> {
    /// The project's `[ota]` answers: method, transport, address.
    pub target: &'a OtaConfig,
    /// The signed image, resolved from the build's `domains.yaml` ---
    /// never a guess at the application's name.
    pub image: &'a Path,
    /// The answer [`OtaStage::ReadState`] produced, once it has.
    pub slot_hash: Option<&'a str>,
    /// The client program. Also the test seam: point it at a fixture.
    pub tool: &'a str,
}

/// One OTA mechanism, as commands.
///
/// The runner walks [`Self::stages`] and asks for each stage's command, so a
/// second mechanism is a new driver and nothing else: no `match` on
/// [`OtaMethod`] outside the registry. `Send + Sync` because the registry
/// hands out shared references from a `static` --- free for the drivers,
/// which are unit structs whose state all arrives in the [`OtaContext`].
pub trait OtaMethodDriver: Send + Sync {
    fn method(&self) -> OtaMethod;

    /// The stages this mechanism runs, in order.
    fn stages(&self) -> &'static [OtaStage];

    /// The command for one stage, or a refusal naming what is missing ---
    /// never a guess (`Err` phrased as a sentence, `Backend::flash_command`'s
    /// rule). A stage the driver does not declare is refused, not improvised.
    fn stage_command(&self, stage: OtaStage, ctx: &OtaContext<'_>) -> Result<Command, String>;

    /// Reads a stage's answer out of its output --- for MCUmgr, the slot
    /// hash a state read reported. Per-mechanism, which is why it lives on
    /// the driver; `None` means the stage has no answer to read.
    fn read_answer(&self, stage: OtaStage, output: &str) -> Option<String>;

    /// One output line's worth of progress, in whichever shape the client
    /// reports it. Defaults to the shared shapes (`ninja`'s counter,
    /// esptool's percentage); a mechanism whose client prints something
    /// else overrides this.
    fn progress(&self, line: &str) -> Option<Progress> {
        crate::progress::detect(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_for_every_variant() {
        // These are written into people's repositories, so a rename is a
        // breaking change and this is the test that notices one.
        for method in OtaMethod::ALL {
            assert_eq!(OtaMethod::from_id(method.id()), Some(*method));
        }
        for transport in Transport::ALL {
            assert_eq!(Transport::from_id(transport.id()), Some(*transport));
        }
    }

    #[test]
    fn an_unknown_id_is_none_rather_than_a_default() {
        // A file naming a mechanism this build does not have must not be
        // silently read as the default one: the user would be told their
        // answer was accepted and then watch a different one run.
        assert_eq!(OtaMethod::from_id("http-pull"), None);
        assert_eq!(Transport::from_id("usb"), None);
    }

    #[test]
    fn the_defaults_are_what_a_zephyr_project_usually_wants() {
        let config = OtaConfig::default();

        assert_eq!(config.method, OtaMethod::Mcumgr);
        assert_eq!(config.transport, Transport::Udp);
        assert_eq!(config.address, None);
    }

    #[test]
    fn every_method_declares_a_tool_and_a_transport() {
        for method in OtaMethod::ALL {
            assert!(!method.required_tools().is_empty(), "{}", method.id());
            assert!(!method.transports().is_empty(), "{}", method.id());
        }
    }
}
