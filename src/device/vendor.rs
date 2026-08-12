//! Labels for USB vendor/product ids commonly seen on MicroPython boards.
//!
//! This is a labeling aid, never a filter: `parse::parse_devices` already
//! decides which ports are USB candidates (`SPEC.md` §8 forbids narrowing
//! that further by guessing), so an unrecognised `vid:pid` still shows up ---
//! it just sorts after the known ones and carries no extra label.

/// `(vid:pid, label)`, lower-case to match [`super::DeviceInfo::vid_pid`].
const KNOWN_VENDORS: &[(&str, &str)] = &[
    ("303a:", "Espressif"),
    ("10c4:ea60", "Silicon Labs CP210x"),
    ("0403:6001", "FTDI"),
    ("1a86:7523", "CH340"),
    ("1a86:55d4", "CH341"),
    ("2e8a:0005", "Raspberry Pi (RP2040)"),
];

/// A human label for a known `vid:pid`, or `None` for anything unrecognised.
///
/// Espressif reassigns the `pid` half of `303a:*` across chip revisions, so
/// that one entry matches on the vendor id alone; every other entry matches
/// the full pair to avoid over-claiming an unrelated device from the same
/// vendor.
pub fn label_for(vid_pid: &str) -> Option<&'static str> {
    lookup(KNOWN_VENDORS, vid_pid)
}

/// `(vid:pid, micropython.org "vendor" filter value)` --- a narrower table
/// than [`KNOWN_VENDORS`]. Only entries where the USB vid:pid identifies an
/// actual board vendor belong here: `303a:*` is Espressif's own vendor id,
/// and `2e8a:0005` is the Raspberry Pi Pico's. A generic USB-serial bridge
/// (CP210x, FTDI, CH340, CH341) is soldered onto boards from dozens of
/// unrelated vendors, so treating it as a vendor filter would wrongly narrow
/// a firmware search away from the real board (`SPEC.md` §9).
const BOARD_VENDORS: &[(&str, &str)] = &[("303a:", "Espressif"), ("2e8a:0005", "Raspberry Pi")];

/// The micropython.org/download/ `vendor=` filter value for `vid_pid`, if
/// the id identifies an actual board vendor rather than a bridge chip.
pub fn board_vendor_for(vid_pid: &str) -> Option<&'static str> {
    lookup(BOARD_VENDORS, vid_pid)
}

/// Shared by [`label_for`] and [`board_vendor_for`]: a `303a:` style entry
/// matches on the vendor id alone (Espressif reassigns the `pid` half across
/// chip revisions), every other entry matches the full pair.
fn lookup(table: &[(&'static str, &'static str)], vid_pid: &str) -> Option<&'static str> {
    let vid_pid = vid_pid.to_ascii_lowercase();
    table
        .iter()
        .find(|(pattern, _)| {
            pattern
                .strip_suffix(':')
                .map_or(*pattern == vid_pid, |vid| {
                    vid_pid.starts_with(&format!("{vid}:"))
                })
        })
        .map(|(_, value)| *value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_known_vendors() {
        assert_eq!(label_for("2e8a:0005"), Some("Raspberry Pi (RP2040)"));
        assert_eq!(label_for("10c4:ea60"), Some("Silicon Labs CP210x"));
    }

    #[test]
    fn matches_espressif_by_vendor_id_alone() {
        assert_eq!(label_for("303a:1001"), Some("Espressif"));
        assert_eq!(label_for("303a:4001"), Some("Espressif"));
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(label_for("2E8A:0005"), Some("Raspberry Pi (RP2040)"));
    }

    #[test]
    fn unknown_vendors_are_not_a_match() {
        assert_eq!(label_for("1234:5678"), None);
    }

    #[test]
    fn board_vendor_recognises_real_board_vendors() {
        assert_eq!(board_vendor_for("303a:1001"), Some("Espressif"));
        assert_eq!(board_vendor_for("2e8a:0005"), Some("Raspberry Pi"));
    }

    #[test]
    fn board_vendor_excludes_generic_bridge_chips() {
        // These are soldered onto boards from many unrelated vendors, so
        // treating them as a vendor filter would wrongly narrow a search.
        for bridge_vid_pid in ["10c4:ea60", "0403:6001", "1a86:7523", "1a86:55d4"] {
            assert_eq!(
                board_vendor_for(bridge_vid_pid),
                None,
                "{bridge_vid_pid} is a bridge chip, not a board vendor"
            );
        }
    }
}
