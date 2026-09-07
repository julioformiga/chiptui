//! The `smpmgr` driver: MCUmgr over SMP, Zephyr's own device management
//! protocol.
//!
//! This feature's `backend::esptool::commands`: every `smpmgr` invocation is
//! built here and nowhere else, so an upstream CLI change is a one-file fix
//! and every literal string is pinned by the tests below. Verified against
//! `smpmgr` 0.19.0's typer CLI (`smpmgr [OPTIONS] COMMAND [ARGS]...`):
//! `--ip`/`--port`/`--ble` are *callback* options and precede the
//! subcommand, the same shape esptool's `--port`/`--chip` have.

use super::{OtaContext, OtaMethod, OtaMethodDriver, OtaStage, Transport};
use crate::process::Command;

pub const PROGRAM: &str = "smpmgr";

/// The payload `Probe` asks the board to echo back. Any string answers the
/// question ("is an SMP server listening?"); a recognizable one makes the
/// log line self-explaining.
const ECHO_PAYLOAD: &str = "chiptui";

/// The MCUmgr update cycle, in order --- the hand-run sequence from
/// `OTA_SETUP.md` as code. `Confirm` is last and deliberately *not* chained
/// into: the runner stops in front of it, because until it runs the next
/// reset reverts.
const STAGES: &[OtaStage] = &[
    OtaStage::Probe,
    OtaStage::Upload,
    OtaStage::ReadState,
    OtaStage::MarkPending,
    OtaStage::Reset,
    OtaStage::Verify,
    OtaStage::Confirm,
];

/// MCUmgr via the `smpmgr` client. A unit struct: everything a stage needs
/// arrives in the [`OtaContext`].
pub struct McumgrDriver;

impl OtaMethodDriver for McumgrDriver {
    fn method(&self) -> OtaMethod {
        OtaMethod::Mcumgr
    }

    fn stages(&self) -> &'static [OtaStage] {
        STAGES
    }

    fn stage_command(&self, stage: OtaStage, ctx: &OtaContext<'_>) -> Result<Command, String> {
        if !self.stages().contains(&stage) {
            return Err(format!("mcumgr has no '{}' stage", stage.label()));
        }
        let base = transport(ctx)?;
        match stage {
            OtaStage::Probe => Ok(base.args(["os", "echo", ECHO_PAYLOAD])),
            OtaStage::Upload => Ok(base
                .args(["image", "upload"])
                .arg(ctx.image.to_string_lossy().into_owned())),
            OtaStage::ReadState | OtaStage::Verify => Ok(base.args(["image", "state-read"])),
            OtaStage::MarkPending => {
                let hash = ctx.slot_hash.ok_or_else(|| {
                    "no slot hash yet --- the 'Read state' stage answers it".to_string()
                })?;
                Ok(base.args(["image", "state-write", hash]))
            }
            OtaStage::Reset => Ok(base.args(["os", "reset"])),
            OtaStage::Confirm => Ok(base.args(["image", "state-write", "--confirm"])),
        }
    }

    /// **No progress shape: an upload reports nothing until it is done.**
    ///
    /// Captured 2026-09-07 against a real board --- 1.3 MB over UDP, 25
    /// seconds, 218 bytes of output, of which the only progress is a single
    /// final frame at `100.0%`. `rich` draws a live bar to a *terminal*; to
    /// a pipe, which is how [`crate::process::ProcessManager::spawn`] always
    /// runs it, it renders once at the end, wrapped to an assumed 80 columns
    /// with the size, rate and ETA ellipsised away. There is no `\r` in it
    /// anywhere, so the carriage-return framing esptool's percentage relies
    /// on does not exist here, and no intermediate value ever arrives.
    ///
    /// So this deliberately answers `None` rather than falling through to
    /// [`crate::progress::detect`]. Reading the one `100.0%` would light the
    /// state line for the instant before the stage finished and tell the
    /// user nothing they were not about to see; and the fall-through is a
    /// hazard, because a shape added later for some other tool would start
    /// matching that frame by accident. The stopwatch is the honest report,
    /// and the way to a real bar is to give `rich` a terminal --- a
    /// different decision, with the whole ANSI-cursor rendering behind it.
    fn progress(&self, _line: &str) -> Option<crate::progress::Progress> {
        None
    }

    fn read_answer(&self, stage: OtaStage, output: &str) -> Option<String> {
        // A state read's answer is the hash sitting in the slot the cycle
        // cares about: slot 1 right after the upload (what `MarkPending`
        // must name), slot 0 after the swap (what `Verify` compares against
        // the hash it armed --- the image keeps its hash across the swap).
        match stage {
            OtaStage::ReadState => slot_hash(output, 1),
            OtaStage::Verify => slot_hash(output, 0),
            _ => None,
        }
    }
}

/// `smpmgr <--ip ADDR | --port DEV | --ble ADDR>` --- the transport flag is
/// a callback option, ahead of the subcommand. A transport with no address
/// refuses by name: there is no default board to talk to.
fn transport(ctx: &OtaContext<'_>) -> Result<Command, String> {
    let target = ctx.target;
    let flag = match target.transport {
        Transport::Udp => "--ip",
        Transport::Serial => "--port",
        Transport::Ble => "--ble",
    };
    let address = target
        .address
        .as_deref()
        .filter(|address| !address.is_empty())
        .ok_or_else(|| {
            format!(
                "no {} configured --- [ota] address in chiptui.toml is unanswered",
                target.transport.address_label()
            )
        })?;
    Ok(Command::new(ctx.tool).arg(flag).arg(address))
}

/// The hash of the image in `slot`, read out of an `image state-read`
/// table.
///
/// smpmgr prints each slot as a rich-pretty `ImageState(...)`, whose `hash`
/// field renders as `hash=HashBytes('...')` over several lines, upper-case
/// hex, and --- on a console narrow enough --- split into several quoted
/// fragments. So the parse is: find the block whose `slot=` matches, then
/// collect every hex digit between `hash=HashBytes(` and its closing `)`.
/// Anything else (`No images on device!`, a `hash=None` from serial
/// recovery, an error printout) is `None`, not a misparse.
fn slot_hash(output: &str, slot: u8) -> Option<String> {
    let needle = format!("slot={slot},");
    let start = output.find(&needle)?;
    let rest = &output[start..];
    let end = rest.find("ImageState(").unwrap_or(rest.len());
    let block = &rest[..end];
    let after = &block[block.find("hash=HashBytes(")? + "hash=HashBytes(".len()..];
    let close = after.find(')')?;
    let hash: String = after[..close]
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect();
    (!hash.is_empty()).then_some(hash)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ota::OtaConfig;

    const ADDR: &str = "192.168.1.42";
    const HASH: &str = "AABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDD";

    fn ctx<'a>(target: &'a OtaConfig, slot_hash: Option<&'a str>) -> OtaContext<'a> {
        OtaContext {
            target,
            image: Path::new("build_ota/app/zephyr/zephyr.signed.bin"),
            slot_hash,
            tool: PROGRAM,
        }
    }

    fn answered() -> OtaConfig {
        OtaConfig {
            address: Some(ADDR.to_string()),
            ..OtaConfig::default()
        }
    }

    #[test]
    fn probe_echoes_a_recognizable_payload() {
        let command = McumgrDriver
            .stage_command(OtaStage::Probe, &ctx(&answered(), None))
            .unwrap();
        assert_eq!(
            command.to_string(),
            "smpmgr --ip 192.168.1.42 os echo chiptui"
        );
    }

    #[test]
    fn upload_names_the_signed_image() {
        let command = McumgrDriver
            .stage_command(OtaStage::Upload, &ctx(&answered(), None))
            .unwrap();
        assert_eq!(
            command.to_string(),
            "smpmgr --ip 192.168.1.42 image upload build_ota/app/zephyr/zephyr.signed.bin"
        );
    }

    #[test]
    fn read_state_and_verify_are_the_same_command() {
        for stage in [OtaStage::ReadState, OtaStage::Verify] {
            let command = McumgrDriver
                .stage_command(stage, &ctx(&answered(), None))
                .unwrap();
            assert_eq!(
                command.to_string(),
                "smpmgr --ip 192.168.1.42 image state-read"
            );
        }
    }

    #[test]
    fn mark_pending_writes_the_read_hash() {
        let command = McumgrDriver
            .stage_command(OtaStage::MarkPending, &ctx(&answered(), Some(HASH)))
            .unwrap();
        assert_eq!(
            command.to_string(),
            format!("smpmgr --ip 192.168.1.42 image state-write {HASH}")
        );
    }

    #[test]
    fn mark_pending_refuses_a_hash_it_does_not_have() {
        let refusal = McumgrDriver
            .stage_command(OtaStage::MarkPending, &ctx(&answered(), None))
            .unwrap_err();
        assert!(
            refusal.contains("no slot hash"),
            "the refusal names what is missing: {refusal}"
        );
    }

    #[test]
    fn reset_and_confirm() {
        let reset = McumgrDriver
            .stage_command(OtaStage::Reset, &ctx(&answered(), None))
            .unwrap();
        assert_eq!(reset.to_string(), "smpmgr --ip 192.168.1.42 os reset");

        let confirm = McumgrDriver
            .stage_command(OtaStage::Confirm, &ctx(&answered(), None))
            .unwrap();
        assert_eq!(
            confirm.to_string(),
            "smpmgr --ip 192.168.1.42 image state-write --confirm"
        );
    }

    #[test]
    fn the_transport_flag_follows_the_configured_transport() {
        for (transport, flag, address) in [
            (Transport::Udp, "--ip", "192.168.1.42"),
            (Transport::Serial, "--port", "/dev/ttyACM0"),
            (Transport::Ble, "--ble", "C4:5B:BE:89:00:00"),
        ] {
            let target = OtaConfig {
                transport,
                address: Some(address.to_string()),
                ..OtaConfig::default()
            };
            let command = McumgrDriver
                .stage_command(OtaStage::Probe, &ctx(&target, None))
                .unwrap();
            assert_eq!(
                &command.args_slice()[..2],
                [flag, address],
                "{flag} precedes the subcommand"
            );
        }
    }

    #[test]
    fn a_transport_without_an_address_refuses_by_name() {
        for (transport, what) in [
            (Transport::Udp, "IP address"),
            (Transport::Serial, "serial port"),
            (Transport::Ble, "Bluetooth address"),
        ] {
            for address in [None, Some(String::new())] {
                let target = OtaConfig {
                    transport,
                    address,
                    ..OtaConfig::default()
                };
                let refusal = McumgrDriver
                    .stage_command(OtaStage::Probe, &ctx(&target, None))
                    .unwrap_err();
                assert!(
                    refusal.contains(what),
                    "{transport:?} without an address names it: {refusal}"
                );
            }
        }
    }

    #[test]
    fn the_tool_path_is_the_test_seam() {
        let target = answered();
        let mut context = ctx(&target, None);
        context.tool = "/fixtures/bin/smpmgr";
        let command = McumgrDriver
            .stage_command(OtaStage::Probe, &context)
            .unwrap();
        assert_eq!(command.program(), "/fixtures/bin/smpmgr");
    }

    /// A two-slot table in the shape smpmgr 0.19.0 prints --- rendered by
    /// the real `smp` 4.1.0 `ImageState` through `rich`'s pretty printer
    /// (what `print(image)` does), with **distinct synthetic hashes**, which
    /// is what makes it the test for telling the slots apart. The captured
    /// board below is the format's witness; this one is the discriminator's,
    /// because a real board that has just been given the same image twice
    /// reports the same hash in both slots and could not tell a slot-0 read
    /// from a slot-1 one.
    const STATE_READ: &str = "\
ImageState(
    slot=0,
    version='0.2.0',
    image=None,
    hash=HashBytes(
        'AABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDD'
    ),
    bootable=True,
    pending=False,
    confirmed=True,
    active=True,
    permanent=False
)
ImageState(
    slot=1,
    version='0.1.0',
    image=None,
    hash=HashBytes(
        '1122334411223344112233441122334411223344112233441122334411223344'
    ),
    bootable=True,
    pending=False,
    confirmed=False,
    active=False,
    permanent=False
)
";

    /// Captured 2026-09-07 from a real board --- a Seeed XIAO ESP32-C3 over
    /// UDP, `smpmgr --ip … image state-read > file 2>&1`, byte for byte.
    ///
    /// What it adds over [`STATE_READ`] is everything *around* the table,
    /// which no hand-written sample had: two `rich` status lines in front
    /// (whose leading spinner frame varies run to run) and a `splitStatus`
    /// line behind. `read_answer` has to walk past all three, and the only
    /// way to know they were there was to run the tool.
    ///
    /// Its two slots carry the same image, so it cannot stand in for
    /// [`STATE_READ`]: it witnesses the format, not the slot arithmetic.
    const STATE_READ_CAPTURED: &str = "\
\u{2819} Connecting to 192.168.1.177... OK
\u{280b} Waiting for image states... OK
ImageState(
    slot=0,
    version='0.2.0',
    image=None,
    hash=HashBytes(
        '40A619CD85601FCACE8DA154F0B875D1A9D31465F052F64C677EFB8D70F9AC0D'
    ),
    bootable=True,
    pending=False,
    confirmed=True,
    active=True,
    permanent=False
)
ImageState(
    slot=1,
    version='0.2.0',
    image=None,
    hash=HashBytes(
        '40A619CD85601FCACE8DA154F0B875D1A9D31465F052F64C677EFB8D70F9AC0D'
    ),
    bootable=True,
    pending=False,
    confirmed=False,
    active=False,
    permanent=False
)
splitStatus: 0
";

    /// The status and `splitStatus` lines a real client wraps the table in
    /// are not an error and are not an answer --- they are skipped.
    #[test]
    fn the_real_clients_status_lines_do_not_confuse_the_read() {
        const HASH: &str = "40A619CD85601FCACE8DA154F0B875D1A9D31465F052F64C677EFB8D70F9AC0D";
        assert_eq!(
            McumgrDriver
                .read_answer(OtaStage::ReadState, STATE_READ_CAPTURED)
                .as_deref(),
            Some(HASH)
        );
        assert_eq!(
            McumgrDriver
                .read_answer(OtaStage::Verify, STATE_READ_CAPTURED)
                .as_deref(),
            Some(HASH)
        );
    }

    #[test]
    fn read_state_answers_the_hash_in_slot_1() {
        let answer = McumgrDriver.read_answer(OtaStage::ReadState, STATE_READ);
        assert_eq!(
            answer.as_deref(),
            Some("1122334411223344112233441122334411223344112233441122334411223344")
        );
    }

    #[test]
    fn verify_answers_the_hash_in_slot_0() {
        let answer = McumgrDriver.read_answer(OtaStage::Verify, STATE_READ);
        assert_eq!(
            answer.as_deref(),
            Some("AABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDD")
        );
    }

    #[test]
    fn a_hash_split_by_a_narrow_console_is_rejoined() {
        // Rich wraps long strings into adjacent quoted fragments when they
        // pass the console width (`COLUMNS` applies even piped) --- the
        // hash must survive that.
        let wrapped = STATE_READ.replace(
            "'1122334411223344112233441122334411223344112233441122334411223344'",
            "'1122334411223344'\n        '112233441122334411223344112233441122334411223344'",
        );
        let answer = McumgrDriver.read_answer(OtaStage::ReadState, &wrapped);
        assert_eq!(
            answer.as_deref(),
            Some("1122334411223344112233441122334411223344112233441122334411223344")
        );
    }

    #[test]
    fn answers_that_are_not_hashes_are_none() {
        assert_eq!(
            McumgrDriver.read_answer(OtaStage::ReadState, "No images on device!"),
            None
        );
        // Serial recovery can answer with the hash field omitted entirely.
        let no_hash = STATE_READ.replace(
            "hash=HashBytes(\n        '1122334411223344112233441122334411223344112233441122334411223344'\n    ),",
            "hash=None,",
        );
        assert_eq!(
            McumgrDriver.read_answer(OtaStage::ReadState, &no_hash),
            None
        );
        // And a stage that has no answer to read never invents one.
        assert_eq!(McumgrDriver.read_answer(OtaStage::Probe, STATE_READ), None);
    }

    #[test]
    fn only_arm_swap_and_confirm_are_destructive() {
        for stage in OtaStage::ALL {
            let expected = matches!(
                stage,
                OtaStage::MarkPending | OtaStage::Reset | OtaStage::Confirm
            );
            assert_eq!(stage.is_destructive(), expected, "{stage:?}");
        }
    }

    #[test]
    fn timeouts_and_settles_follow_the_stage_not_the_tool() {
        for stage in OtaStage::ALL {
            let timeout = stage.timeout();
            match stage {
                OtaStage::Upload => assert!(
                    timeout >= std::time::Duration::from_secs(120),
                    "an upload over a bad link is minutes: {timeout:?}"
                ),
                _ => assert!(
                    timeout <= std::time::Duration::from_secs(60),
                    "{stage:?} answers instantly or not at all: {timeout:?}"
                ),
            }
        }
        // Measured, not guessed: the reference board answered 58 s after
        // `os reset` for a 1.3 MB image, so the settle has to clear that
        // with room --- a shorter one fails `Verify` on an update that
        // worked. See `OtaStage::settle`.
        assert!(
            OtaStage::Reset.settle() >= std::time::Duration::from_secs(75),
            "the measured swap was 58 s and scales with image size: {:?}",
            OtaStage::Reset.settle()
        );
        for stage in OtaStage::ALL {
            if *stage != OtaStage::Reset {
                assert_eq!(stage.settle(), std::time::Duration::ZERO, "{stage:?}");
            }
        }
    }
}
