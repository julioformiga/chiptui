//! Parsers for `mpremote` output.
//!
//! Formats confirmed against mpremote 1.28.0 (`commands.py`):
//!
//! ```text
//! ls        "{st_size:12} {name}"  + "/" when the entry is a directory
//! devs      "{port} {serial} {vid:04x}:{pid:04x} {manufacturer} {product}"
//! sha256sum "{digest_hex}"
//! errors    "mpremote: <message>" on stderr, exit code 1
//! ```
//!
//! `SPEC.md` §23 warns that these can change between versions, so parsing is
//! tolerant: a line that does not fit is collected as unparsed and surfaced as
//! a warning, never as a failed listing.

use crate::device::DeviceInfo;

/// One entry in a device directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// The result of parsing one `ls`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<RemoteEntry>,
    /// Lines that did not match the expected format.
    pub unparsed: Vec<String>,
}

pub fn parse_listing(stdout: &str) -> Listing {
    let mut listing = Listing::default();

    for line in stdout.lines() {
        if line.trim().is_empty() || is_verbose_header(line) {
            continue;
        }
        match parse_entry(line) {
            Some(entry) => listing.entries.push(entry),
            None => listing.unparsed.push(line.to_string()),
        }
    }

    listing
}

/// Matches the `ls :/lib` banner mpremote prints unless `--no-verbose` is given.
///
/// Recognised explicitly so that running against a version without the flag
/// degrades to a correct listing instead of a spurious warning.
fn is_verbose_header(line: &str) -> bool {
    line.split_once(' ').is_some_and(|(command, rest)| {
        !command.is_empty()
            && command.chars().all(|c| c.is_ascii_alphanumeric())
            && rest.starts_with(':')
    })
}

fn parse_entry(line: &str) -> Option<RemoteEntry> {
    let trimmed = line.trim_start_matches(' ');
    // The size field is right-aligned in 12 columns, but a large file overflows
    // it, so the digits are located rather than sliced at a fixed offset.
    let digits = trimmed.len()
        - trimmed
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    if digits == 0 {
        return None;
    }
    let (size, rest) = trimmed.split_at(digits);
    let size = size.parse().ok()?;

    // Exactly one space separates size and name: consuming whitespace greedily
    // would corrupt names that legitimately begin with a space.
    let name = rest.strip_prefix(' ')?;
    if name.is_empty() {
        return None;
    }

    match name.strip_suffix('/') {
        Some(name) if !name.is_empty() => Some(RemoteEntry {
            name: name.to_string(),
            size,
            is_dir: true,
        }),
        _ => Some(RemoteEntry {
            name: name.to_string(),
            size,
            is_dir: false,
        }),
    }
}

/// Placeholder `mpremote` prints when a port reports no USB vendor/product id.
const NOT_USB: &str = "0000:0000";

/// Parses `mpremote devs` output, keeping only USB serial devices.
///
/// `devs` lists *every* comport, which on a typical Linux box means 32 legacy
/// `/dev/ttyS*` UARTs before any board. mpremote's own auto-connect skips ports
/// without a vendor/product id (`commands.py:41`), and so does this: a
/// MicroPython board is always a USB device.
pub fn parse_devices(stdout: &str) -> Vec<DeviceInfo> {
    stdout
        .lines()
        .filter_map(parse_device)
        .filter(|device| device.vid_pid != NOT_USB)
        .collect()
}

fn parse_device(line: &str) -> Option<DeviceInfo> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // Manufacturer and product both contain spaces, so only the first three
    // fields are split off; the rest is the description.
    let mut fields = line.splitn(4, ' ');
    let port = fields.next()?;
    let serial = fields.next()?;
    let vid_pid = fields.next()?;
    // Structural check against stray output: the third field is always vid:pid.
    if !vid_pid.contains(':') {
        return None;
    }

    Some(DeviceInfo {
        port: port.to_string(),
        // Ports without a USB serial number print Python's `None`.
        serial: (serial != "None").then(|| serial.to_string()),
        vid_pid: vid_pid.to_string(),
        description: clean_description(fields.next().unwrap_or("")),
    })
}

/// Drops the `None` placeholders a missing manufacturer or product leaves behind.
fn clean_description(text: &str) -> String {
    text.split_whitespace()
        .filter(|word| *word != "None")
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extracts the digest from `mpremote fs sha256sum` output.
pub fn parse_sha256(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

/// What kind of failure `mpremote`'s stderr describes.
///
/// A single classifier backs both [`explain_error`] (the message) and
/// [`is_device_lost_error`] (whether a rescan should follow) so the substring
/// table lives in one place.
enum FailureKind {
    /// No board answered at all.
    DeviceNotFound,
    /// A board was seen but stopped responding mid-command.
    DeviceUnresponsive,
    PermissionDenied,
    PathNotFound,
    Empty,
    Other,
}

fn classify(stderr: &str) -> FailureKind {
    let lower = stderr.trim().to_ascii_lowercase();

    if lower.is_empty() {
        FailureKind::Empty
    } else if lower.contains("no device found") || lower.contains("no serial device") {
        FailureKind::DeviceNotFound
    } else if lower.contains("failed to access") {
        FailureKind::DeviceUnresponsive
    } else if lower.contains("permission denied") || lower.contains("could not open port") {
        FailureKind::PermissionDenied
    } else if lower.contains("no such file") || lower.contains("enoent") {
        FailureKind::PathNotFound
    } else {
        FailureKind::Other
    }
}

/// Turns `mpremote`'s stderr into something the user can act on.
///
/// `AGENTS.md` §Error Messages: say what failed and what to do next. The raw
/// text stays in the log pane either way.
pub fn explain_error(stderr: &str) -> String {
    match classify(stderr) {
        FailureKind::DeviceNotFound => {
            "no MicroPython device found — connect the board, then press 'd' to rescan"
                .to_string()
        }
        FailureKind::DeviceUnresponsive => {
            "the device is present but did not respond — try unplugging and reconnecting it"
                .to_string()
        }
        FailureKind::PermissionDenied => "cannot open the serial port — check that no other program holds it, and that your user is in the 'dialout' group".to_string(),
        FailureKind::PathNotFound => "that path does not exist on the device".to_string(),
        FailureKind::Empty => "mpremote failed without reporting a reason".to_string(),
        // Already prefixed with "mpremote: " by the tool itself.
        FailureKind::Other => stderr.trim().to_string(),
    }
}

/// Whether this failure means the device is gone, so a fresh `devs` scan
/// should run rather than leaving a stale selection pointing at a dead port.
pub fn is_device_lost_error(stderr: &str) -> bool {
    matches!(classify(stderr), FailureKind::DeviceNotFound)
}

/// Whether the device is present but did not respond to the command.
/// This often happens when a board is connected but does not have MicroPython installed.
pub fn is_device_unresponsive_error(stderr: &str) -> bool {
    matches!(classify(stderr), FailureKind::DeviceUnresponsive)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output shaped exactly like `"{st_size:12} {name}"`.
    const LS_OUTPUT: &str = "         139 boot.py\n        1024 main.py\n           0 lib/\n";

    #[test]
    fn parses_files_and_directories() {
        let listing = parse_listing(LS_OUTPUT);

        assert!(listing.unparsed.is_empty());
        assert_eq!(
            listing.entries,
            vec![
                RemoteEntry {
                    name: "boot.py".into(),
                    size: 139,
                    is_dir: false
                },
                RemoteEntry {
                    name: "main.py".into(),
                    size: 1024,
                    is_dir: false
                },
                RemoteEntry {
                    name: "lib".into(),
                    size: 0,
                    is_dir: true
                },
            ]
        );
    }

    #[test]
    fn tolerates_the_verbose_header() {
        // Running against a version without --no-verbose must still work.
        let listing = parse_listing(&format!("ls :/lib\n{LS_OUTPUT}"));
        assert_eq!(listing.entries.len(), 3);
        assert!(listing.unparsed.is_empty(), "the banner is not a warning");
    }

    #[test]
    fn keeps_spaces_inside_names() {
        let listing = parse_listing("         512 my data.txt\n           0 my dir/\n");
        assert_eq!(listing.entries[0].name, "my data.txt");
        assert_eq!(listing.entries[1].name, "my dir");
        assert!(listing.entries[1].is_dir);
    }

    #[test]
    fn a_name_beginning_with_a_space_survives() {
        // Only one space is consumed, so the second belongs to the name.
        let listing = parse_listing("          12  leading.py");
        assert_eq!(listing.entries[0].name, " leading.py");
    }

    #[test]
    fn large_sizes_overflow_the_column_without_breaking() {
        let listing = parse_listing("1234567890123 firmware.bin");
        assert_eq!(listing.entries[0].size, 1_234_567_890_123);
        assert_eq!(listing.entries[0].name, "firmware.bin");
    }

    #[test]
    fn malformed_lines_are_collected_not_fatal() {
        let listing =
            parse_listing("         139 boot.py\n?? unexpected ??\n        1024 main.py\n");

        assert_eq!(listing.entries.len(), 2, "good lines still parse");
        assert_eq!(listing.unparsed, vec!["?? unexpected ??"]);
    }

    #[test]
    fn empty_output_is_an_empty_directory() {
        assert_eq!(parse_listing(""), Listing::default());
        assert_eq!(parse_listing("\n\n"), Listing::default());
    }

    #[test]
    fn parses_the_device_list() {
        let devices = parse_devices(
            "/dev/ttyACM0 e6614104036e5f24 2e8a:0005 MicroPython Board in FS mode\n\
             /dev/ttyUSB0 None 10c4:ea60 Silicon Labs CP2102 USB to UART Bridge Controller\n",
        );

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].port, "/dev/ttyACM0");
        assert_eq!(devices[0].serial.as_deref(), Some("e6614104036e5f24"));
        assert_eq!(devices[0].vid_pid, "2e8a:0005");
        assert_eq!(devices[0].description, "MicroPython Board in FS mode");
        assert_eq!(devices[1].serial, None, "'None' means no serial number");
    }

    #[test]
    fn legacy_serial_ports_are_filtered_out() {
        // Verbatim from `mpremote devs` on a machine with no board attached:
        // 32 of these precede any real device.
        let devices = parse_devices(
            "/dev/ttyS0 None 0000:0000 None None\n\
             /dev/ttyS1 None 0000:0000 None None\n\
             /dev/ttyACM0 e6614104 2e8a:0005 MicroPython Board in FS mode\n",
        );

        assert_eq!(devices.len(), 1, "only the USB device is a candidate");
        assert_eq!(devices[0].port, "/dev/ttyACM0");
    }

    #[test]
    fn device_lines_without_a_vid_pid_are_ignored() {
        let devices = parse_devices("some unrelated log line\n/dev/ttyACM0 None 2e8a:0005 Board\n");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].port, "/dev/ttyACM0");
    }

    #[test]
    fn missing_manufacturer_placeholders_are_dropped() {
        let devices = parse_devices("/dev/ttyACM0 None 2e8a:0005 None None\n");
        assert_eq!(devices[0].description, "");
        assert_eq!(devices[0].label(), "/dev/ttyACM0");
    }

    #[test]
    fn extracts_a_digest() {
        let digest = "a".repeat(64);
        assert_eq!(parse_sha256(&digest), Some(digest.clone()));
        // Also when a banner precedes it.
        assert_eq!(
            parse_sha256(&format!("sha256sum :/main.py\n{digest}\n")),
            Some(digest)
        );
    }

    #[test]
    fn rejects_output_that_is_not_a_digest() {
        assert_eq!(parse_sha256(""), None);
        assert_eq!(parse_sha256("not a hash"), None);
        assert_eq!(parse_sha256(&"a".repeat(63)), None);
        assert_eq!(parse_sha256(&"z".repeat(64)), None);
    }

    #[test]
    fn known_failures_become_actionable_advice() {
        assert!(explain_error("mpremote: no device found").contains("connect the board"));
        assert!(
            explain_error("could not open port /dev/ttyACM0: Permission denied")
                .contains("dialout")
        );
        assert!(
            explain_error("mpremote: ls: No such file or directory.").contains("does not exist")
        );
        assert!(!explain_error("").is_empty());
    }

    #[test]
    fn unknown_failures_are_passed_through_verbatim() {
        let message = "mpremote: something entirely new went wrong";
        assert_eq!(explain_error(message), message);
    }

    #[test]
    fn classifies_lost_device() {
        assert!(is_device_lost_error("mpremote: no device found"));
        assert!(is_device_unresponsive_error(
            "mpremote: failed to access /dev/ttyACM0"
        ));
        assert!(!is_device_lost_error(
            "could not open port /dev/ttyACM0: Permission denied"
        ));
        assert!(!is_device_lost_error(
            "mpremote: ls: No such file or directory."
        ));
        assert!(!is_device_lost_error(""));
    }
}
