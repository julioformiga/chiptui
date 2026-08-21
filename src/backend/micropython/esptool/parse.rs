//! Parsers for `esptool` output.
//!
//! Every esptool sub-command that talks to a board prints a "Detecting chip
//! type..."/"Chip is ESP32-..." banner while connecting, so the chip family
//! can be read off any successful command's stdout --- no dedicated probe is
//! needed. Tolerant by construction, same spirit as `micropython::parse`:
//! esptool's exact wording has drifted across major versions, so this matches
//! on family name substrings rather than a fixed line shape.

use super::{ChipFamily, DeviceDetails};

/// Recognised family name, longest/most specific first so `"ESP32-S3"` is not
/// mistaken for a bare `"ESP32"`.
const FAMILY_MARKERS: &[(&str, ChipFamily)] = &[
    ("ESP32-S2", ChipFamily::Esp32S2),
    ("ESP32S2", ChipFamily::Esp32S2),
    ("ESP32-S3", ChipFamily::Esp32S3),
    ("ESP32S3", ChipFamily::Esp32S3),
    ("ESP32-C3", ChipFamily::Esp32C3),
    ("ESP32C3", ChipFamily::Esp32C3),
    ("ESP32-C6", ChipFamily::Esp32C6),
    ("ESP32C6", ChipFamily::Esp32C6),
    ("ESP8266", ChipFamily::Esp8266),
    ("ESP32", ChipFamily::Esp32),
];

/// Looks for a recognised chip family anywhere in `stdout`.
pub fn parse_chip_family(stdout: &str) -> Option<ChipFamily> {
    let upper = stdout.to_ascii_uppercase();
    FAMILY_MARKERS
        .iter()
        .find(|(marker, _)| upper.contains(marker))
        .map(|(_, family)| *family)
}

/// Reads every device fact this module knows how to recognise off `stdout`,
/// for the Dashboard's device panel. Every esptool sub-command that reaches
/// the board prints the same connection banner (chip/features/crystal/MAC),
/// so this needs no dedicated probe --- same reasoning as
/// [`parse_chip_family`], just wider. Tolerant by construction: a field
/// esptool did not print is `None`, not an error, since the caller
/// ([`crate::flash::FlashPanel`]) merges results across several runs.
pub fn parse_device_details(stdout: &str) -> DeviceDetails {
    DeviceDetails {
        family: parse_chip_family(stdout),
        revision: parse_revision(stdout),
        features: parse_field(stdout, "Features:"),
        crystal_mhz: parse_field(stdout, "Crystal is"),
        mac: parse_field(stdout, "MAC:"),
        flash_manufacturer: parse_field(stdout, "Manufacturer:"),
        flash_device: parse_field(stdout, "Device:"),
        flash_size: parse_field(stdout, "Detected flash size:"),
        // Never read off a banner: only the flash-identification probe
        // (`crate::flash::FlashPanel::query_firmware_identity`) sets this,
        // so merging stdout can only ever leave it untouched.
        firmware: None,
    }
}

/// The first line starting with `prefix`, with the prefix and surrounding
/// whitespace stripped. `None` for a blank value, same as an absent line ---
/// a field esptool printed but left empty is not useful to show either.
fn parse_field(stdout: &str, prefix: &str) -> Option<String> {
    let line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with(prefix))?;
    let value = line.trim_start().strip_prefix(prefix)?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `"Chip is ESP32-D0WD (revision 3)"` -> `"3"`. Kept separate from
/// [`parse_field`] because the revision sits mid-line, inside parentheses,
/// rather than after a line-leading label.
fn parse_revision(stdout: &str) -> Option<String> {
    let line = stdout.lines().find(|line| line.contains("revision"))?;
    let after = line.split("revision").nth(1)?;
    let value = after.trim().trim_end_matches(')').trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Turns esptool's stderr into something the user can act on, the raw text
/// staying in the log pane either way (`AGENTS.md` §Error Messages).
pub fn explain_error(stderr: &str) -> String {
    let raw = stderr.trim();
    let lower = raw.to_ascii_lowercase();

    if lower.contains("could not open") || lower.contains("permission denied") {
        "cannot open the serial port — check that no other program holds it, and that your user is in the 'dialout' group".to_string()
    } else if lower.contains("no such file") {
        "the firmware file could not be found".to_string()
    } else if lower.contains("timed out") || lower.contains("no serial data received") {
        "the device did not respond — check the port and that it is in bootloader mode".to_string()
    } else if raw.is_empty() {
        "esptool failed without reporting a reason".to_string()
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_families_from_a_realistic_banner() {
        assert_eq!(
            parse_chip_family("Detecting chip type... ESP32\nChip is ESP32-D0WD (revision 3)\n"),
            Some(ChipFamily::Esp32)
        );
        assert_eq!(
            parse_chip_family("Chip is ESP32-S3 (revision 0)"),
            Some(ChipFamily::Esp32S3)
        );
        assert_eq!(
            parse_chip_family("Chip is ESP32-C3 (QFN32) (revision 3)"),
            Some(ChipFamily::Esp32C3)
        );
        assert_eq!(
            parse_chip_family("Chip is ESP8266EX"),
            Some(ChipFamily::Esp8266)
        );
    }

    #[test]
    fn a_specific_family_is_not_mistaken_for_the_bare_one() {
        assert_eq!(
            parse_chip_family("Chip is ESP32-S2FH4 (revision 0)"),
            Some(ChipFamily::Esp32S2)
        );
    }

    #[test]
    fn unrecognised_output_yields_nothing() {
        assert_eq!(parse_chip_family("Writing at 0x00010000... (50 %)"), None);
        assert_eq!(parse_chip_family(""), None);
    }

    #[test]
    fn known_failures_become_actionable_advice() {
        assert!(
            explain_error("Could not open /dev/ttyUSB0, the port is busy or doesn't exist")
                .contains("dialout")
        );
        assert!(explain_error("").contains("without reporting a reason"));
    }

    #[test]
    fn unknown_failures_are_passed_through_verbatim() {
        let message = "A fatal error occurred: something entirely new";
        assert_eq!(explain_error(message), message);
    }

    #[test]
    fn device_details_reads_the_full_connection_banner() {
        let stdout = "esptool v5.3.1\n\
             Serial port /dev/ttyUSB0\n\
             Detecting chip type... ESP32\n\
             Chip is ESP32-D0WD (revision 3)\n\
             Features: Wi-Fi, BT, Dual Core + LP Core, 240MHz, Coding Scheme None\n\
             Crystal is 40MHz\n\
             MAC: 24:6f:28:12:34:56\n";
        let details = parse_device_details(stdout);
        assert_eq!(details.family, Some(ChipFamily::Esp32));
        assert_eq!(details.revision.as_deref(), Some("3"));
        assert_eq!(
            details.features.as_deref(),
            Some("Wi-Fi, BT, Dual Core + LP Core, 240MHz, Coding Scheme None")
        );
        assert_eq!(details.crystal_mhz.as_deref(), Some("40MHz"));
        assert_eq!(details.mac.as_deref(), Some("24:6f:28:12:34:56"));
        assert_eq!(details.flash_manufacturer, None);
    }

    #[test]
    fn device_details_reads_flash_geometry() {
        let stdout = "Chip is ESP32-D0WD (revision 3)\n\
             Manufacturer: 5e\n\
             Device: 4016\n\
             Detected flash size: 4MB\n";
        let details = parse_device_details(stdout);
        assert_eq!(details.flash_manufacturer.as_deref(), Some("5e"));
        assert_eq!(details.flash_device.as_deref(), Some("4016"));
        assert_eq!(details.flash_size.as_deref(), Some("4MB"));
        assert_eq!(details.mac, None, "this banner never mentioned a MAC");
    }

    #[test]
    fn device_details_from_output_with_nothing_recognisable_is_all_none() {
        let details = parse_device_details("Writing at 0x00010000... (50 %)");
        assert_eq!(details, DeviceDetails::default());
        assert!(details.is_empty());
    }

    #[test]
    fn merge_only_overwrites_fields_the_newer_run_actually_reported() {
        let mut details = parse_device_details(
            "Chip is ESP32-D0WD (revision 3)\nManufacturer: 5e\nDevice: 4016\n",
        );
        // A later `chip-id` run mentions the chip again but says nothing
        // about flash --- the earlier flash geometry must survive.
        details.merge(parse_device_details(
            "Chip is ESP32-D0WD (revision 3)\nMAC: 24:6f:28:12:34:56\n",
        ));
        assert_eq!(details.mac.as_deref(), Some("24:6f:28:12:34:56"));
        assert_eq!(details.flash_manufacturer.as_deref(), Some("5e"));
    }
}
