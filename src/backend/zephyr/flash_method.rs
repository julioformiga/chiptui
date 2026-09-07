//! Which way firmware reaches the board: the wired path, or over the air.
//!
//! `Flash` is the one door (`SPEC.md` §10). Pressing it does not start a
//! command --- it asks *how*, because a Zephyr project that carries an
//! `[ota]` section has two honest answers and nothing on the Actions pane
//! says which one the next press would take. The question is asked every
//! time, the way [`crate::app::Overlay::BuildTarget`] asks where a build
//! runs: repeating an answer is `Enter`, changing it is one arrow.
//!
//! Both rows are always drawn, even when only one of them can run. That is
//! the whole reason the menu opens on a project with a single viable path:
//! a dimmed row *with its reason under it* is the only place the user reads
//! why the other way is unavailable. A menu that silently skipped itself
//! would take that sentence away exactly when it is needed.
//!
//! This module is the single definition of those rows --- label, detail and
//! enabled-ness together, from facts handed in rather than read here. The
//! renderer, the key handler and the click handler all consume the same
//! call, which is the discipline [`crate::install::Installer::action`]
//! exists to keep: a label decided in `ui` and an effect decided in `app`
//! is precisely how a dimmed button with a live action behind it ships.

use super::flash_plan::{FlashImage, FlashPlan};
use crate::ota::Transport;

/// The rows this menu has. Fixed: the two ways a Zephyr image reaches a
/// board. Exported so the popup's geometry and the click hit-testing count
/// the same rows the renderer draws.
pub const COUNT: usize = 2;

/// One way of writing firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashMethod {
    /// `west flash`, or the `esptool` image list [`FlashPlan`] resolves for
    /// the one runner family where delegating is broken.
    Usb,
    /// The OTA modal, which decides prepare-vs-update on its own.
    Ota,
}

/// A drawn row: what it says, and whether `Enter` does anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodRow {
    pub method: FlashMethod,
    pub label: String,
    /// The muted second line: what the row does while it can run, why it
    /// cannot while it is disabled. A disabled row is never left to the
    /// dimming alone to explain itself.
    pub detail: String,
    pub enabled: bool,
}

/// What the two rows are decided from. Every field is passed in: this
/// module reads no configuration and touches no filesystem, so all three
/// rules below are unit-testable in memory (`DirScan`'s bias, applied to a
/// menu).
#[derive(Debug, Clone)]
pub struct Facts<'a> {
    /// Whether any USB serial device is present at all.
    ///
    /// Deliberately not "is a port selected": with several boards known and
    /// none picked yet, the device picker is what answers that, and dimming
    /// the wired row meanwhile would refuse a flash the user can perfectly
    /// well perform.
    pub connected: bool,
    /// The selected port, named in the wired row's detail when there is one.
    pub port: Option<&'a str>,
    /// The project's `[ota] transport`, which decides whether the OTA half
    /// depends on the USB cable at all.
    pub transport: Transport,
    /// What [`super::flash_plan::plan`] resolved for the board's build
    /// directory.
    pub plan: Result<FlashPlan, String>,
}

/// The menu's rows, in drawn order.
pub fn rows(facts: &Facts) -> [MethodRow; COUNT] {
    [usb_row(facts), ota_row(facts)]
}

/// Where the cursor opens: the first row that can actually run, so the
/// reflex `Enter` never lands on a dimmed one. All-disabled (a serial-only
/// OTA project with nothing plugged in) falls back to the first row, whose
/// reason is then what the reflex reads.
pub fn first_enabled(rows: &[MethodRow; COUNT]) -> usize {
    rows.iter().position(|row| row.enabled).unwrap_or(0)
}

fn usb_row(facts: &Facts) -> MethodRow {
    // The plan's own `Err` does *not* disable the row. It is a sentence
    // about the build directory, and the confirm behind this row already
    // shows it where it would otherwise show the command --- saying it
    // twice, in two different places, would read as two problems.
    let detail = if facts.connected {
        let what = match &facts.plan {
            Ok(FlashPlan::Delegate) => "west flash — the board's own runner".to_string(),
            Ok(FlashPlan::Images(images)) => format!("esptool: {}", images_summary(images)),
            Err(_) => "the images are unresolved — the confirm says why".to_string(),
        };
        match facts.port {
            Some(port) => format!("{what} · {port}"),
            None => what,
        }
    } else {
        "no device connected".to_string()
    };
    MethodRow {
        method: FlashMethod::Usb,
        label: "Flash over USB".to_string(),
        detail,
        enabled: facts.connected,
    }
}

fn ota_row(facts: &Facts) -> MethodRow {
    // Only the serial transport rides the cable this menu asks about. A
    // board updated over UDP or BLE has nothing to do with what is plugged
    // into *this* machine, and dimming its row for an empty USB bus would
    // be ChipTUI inventing a dependency the mechanism does not have.
    let enabled = facts.transport != Transport::Serial || facts.connected;
    let detail = if enabled {
        format!(
            "prepare, then push signed images over {}",
            facts.transport.label()
        )
    } else {
        "the OTA transport is serial — no device connected".to_string()
    };
    MethodRow {
        method: FlashMethod::Ota,
        // The transport rides the label so a dimmed serial row explains
        // itself before its detail line is read.
        label: format!("OTA update ({})", facts.transport.id()),
        detail,
        enabled,
    }
}

/// `mcuboot @0x0 + app @0x20000` --- what the one `esptool` invocation
/// writes, so the row says where the images land before the confirm quotes
/// the command.
fn images_summary(images: &[FlashImage]) -> String {
    images
        .iter()
        .map(|image| format!("{} @{:#x}", image.domain, image.address))
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn facts<'a>(connected: bool, transport: Transport) -> Facts<'a> {
        Facts {
            connected,
            port: connected.then_some("/dev/ttyACM0"),
            transport,
            plan: Ok(FlashPlan::Delegate),
        }
    }

    #[test]
    fn with_a_board_plugged_in_both_rows_run() {
        let rows = rows(&facts(true, Transport::Udp));
        assert!(rows.iter().all(|row| row.enabled));
        assert_eq!(rows[0].label, "Flash over USB");
        assert!(rows[0].detail.contains("west flash"));
        assert!(rows[0].detail.contains("/dev/ttyACM0"));
        assert_eq!(rows[1].label, "OTA update (udp)");
        assert_eq!(first_enabled(&rows), 0);
    }

    #[test]
    fn an_empty_usb_bus_disables_the_wired_row_and_says_so() {
        let rows = rows(&facts(false, Transport::Udp));
        assert!(!rows[0].enabled);
        assert_eq!(rows[0].detail, "no device connected");
        // The network transport needs no cable, so the OTA half is a real
        // action and the cursor opens on it.
        assert!(rows[1].enabled);
        assert_eq!(first_enabled(&rows), 1);
    }

    #[test]
    fn a_serial_transport_makes_ota_depend_on_the_cable_too() {
        let unplugged = rows(&facts(false, Transport::Serial));
        assert!(!unplugged[1].enabled);
        assert_eq!(
            unplugged[1].detail,
            "the OTA transport is serial — no device connected"
        );
        // Nothing can run: the cursor stays on the first row, whose reason
        // is what a reflex `Enter` reads.
        assert_eq!(first_enabled(&unplugged), 0);

        let plugged = rows(&facts(true, Transport::Serial));
        assert!(plugged[1].enabled);
    }

    #[test]
    fn ble_never_asks_about_usb() {
        let rows = rows(&facts(false, Transport::Ble));
        assert!(rows[1].enabled);
        assert!(rows[1].detail.ends_with("over BLE"));
    }

    #[test]
    fn an_esptool_plan_names_where_the_images_land() {
        let mut facts = facts(true, Transport::Udp);
        facts.plan = Ok(FlashPlan::Images(vec![
            FlashImage {
                domain: "mcuboot".to_string(),
                path: PathBuf::from("mcuboot/zephyr/zephyr.bin"),
                address: 0x0,
            },
            FlashImage {
                domain: "app".to_string(),
                path: PathBuf::from("app/zephyr/zephyr.signed.bin"),
                address: 0x2_0000,
            },
        ]));
        let rows = rows(&facts);
        assert!(
            rows[0]
                .detail
                .starts_with("esptool: mcuboot @0x0 + app @0x20000")
        );
    }

    #[test]
    fn an_unresolvable_plan_still_runs_the_row() {
        let mut facts = facts(true, Transport::Udp);
        facts.plan = Err("the devicetree has no boot_partition".to_string());
        let rows = rows(&facts);
        // The refusal belongs to the confirm, which shows it where the
        // command would go; the row only says it is coming.
        assert!(rows[0].enabled);
        assert!(rows[0].detail.contains("the confirm says why"));
    }
}
