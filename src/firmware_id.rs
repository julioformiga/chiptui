//! Identifying which firmware is installed on a connected board by reading
//! its flash.
//!
//! The cheapest honest answer to "what is this board running?" is the flash
//! itself: the partition table carries labels an OS image leaves behind
//! (Zephyr's `mcuboot`/`slot0_partition`), the start of the application
//! area carries the build's banner strings (`MicroPython v1…`, `Booting
//! Zephyr OS`) and every ESP-IDF application embeds its `esp_app_desc`
//! magic. [`identify`] is a pure function over the bytes `esptool
//! read-flash` brought back, so every rule here is unit-testable in memory.
//! A window that is entirely `0xFF` answers differently from an
//! unrecognized one: [`classify`] reports erased flash --- a device with
//! no firmware on it at all --- and a named firmware carries the version
//! it names itself with ([`version`]): the banner string for MicroPython
//! and Zephyr, the app descriptor's stamped fields for ESP-IDF.

/// What the flash contents say the board is running. The two backends
/// ChipTUI knows how to drive, plus the ESP-IDF app neither of them is ---
/// still worth naming, since it is what the flash actually says and it is
/// detectable with the same read. Anything else reads as `None`
/// (`undefined` in the Device info pane) rather than a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashFirmware {
    MicroPython,
    Zephyr,
    /// A plain ESP-IDF application. Detected by the `esp_app_desc_t`
    /// magic word in the *app* region only --- the ESP-IDF second-stage
    /// bootloader is shared by all three firmwares (MicroPython and
    /// Zephyr both build on it), so bootloader bytes classify nothing.
    EspIdf,
}

impl FlashFirmware {
    pub const fn label(self) -> &'static str {
        match self {
            Self::MicroPython => "MicroPython",
            Self::Zephyr => "Zephyr",
            Self::EspIdf => "ESP-IDF",
        }
    }
}

/// The identification read's full verdict. A named firmware is one
/// answer, but so is proof the flash is erased: "no firmware installed"
/// is different from `None` (never asked, declined, or nothing
/// recognizable) and worth reporting as what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirmwareVerdict {
    /// The firmware the window names, plus the version it names itself
    /// with when the same bytes carry one (`Firmware: Zephyr v4.0.0`).
    /// `None` is honest: labels identify without any version in reach,
    /// and a guessed version is worse than a bare name.
    Firmware(FlashFirmware, Option<String>),
    /// The whole identification window reads `0xFF`: erased flash, a
    /// chip that never had firmware written to it (or was erased since).
    Erased,
}

/// What erased NOR flash reads as, on every byte.
const ERASED: u8 = 0xFF;

/// Where the identification read starts: the very beginning of flash. On
/// a Zephyr sysbuild image the bootloader region (below 0x8000) is
/// MCUboot's home, and that is where the build names itself --- verified
/// on hardware, where the Zephyr banner string sits below 0x8000 while
/// the app window stays silent. Starting at 0x0 also keeps the read
/// meaningful on ESP8266 (no partition table convention; the image lives
/// at 0x0).
pub const READ_OFFSET: usize = 0x0;
/// How much flash the identification read covers --- the bootloader
/// (0x0–0x8000), the partition table (0x8000) and the first 64 KiB of the
/// conventional application area (0x10000). Enough for partition labels
/// and for the banners of an MCUboot sysbuild (which names itself in the
/// bootloader) and a MicroPython app (banner at the image start); far
/// cheaper than reading the whole chip.
pub const READ_SIZE: usize = 0x20000;
/// Where the follow-up version hunt reads from when the identification
/// window named a firmware without a version: right past it. A Zephyr
/// *simple boot* image is one contiguous XIP image whose application
/// banner (`*** Booting Zephyr OS build … ***`) lands deep in flash, and
/// how deep tracks the app's own size --- a bare sample app (verified on
/// hardware, ESP32-C3) sat at 0x6053c, but a graphics-heavy one (a round
/// display driven by LVGL, same chip, `zephyr.bin` past 1 MiB) pushed it
/// to 0xd06a8, past the original 512 KiB budget. The hunt covers the next
/// 1 MiB instead, wide enough for both with room to grow; a build that
/// names itself deeper than that stays bare rather than guessed at, and a
/// failed hunt changes nothing.
pub const HUNT_OFFSET: usize = READ_OFFSET + READ_SIZE;
pub const HUNT_SIZE: usize = 0x100000;

/// One partition-table entry is 32 bytes: magic, type, subtype, offset,
/// size, a 16-byte NUL-padded label, then flags.
const ENTRY_SIZE: usize = 32;
const LABEL_OFFSET: usize = 12;
const LABEL_LEN: usize = 16;
/// ESP-IDF's partition-table magic byte (`PT_MAGIC`).
const ENTRY_MAGIC: u8 = 0xAA;
/// Where the partition table sits inside the identification window:
/// [`READ_OFFSET`] plus the ESP convention's 0x8000.
const TABLE_OFFSET_IN_WINDOW: usize = 0x8000 - READ_OFFSET;

/// Labels only a Zephyr (sysbuild/MCUboot) layout produces.
const ZEPHYR_LABELS: [&str; 3] = ["mcuboot", "slot0_partition", "slot1_partition"];

/// Where the conventional application area starts inside the window
/// (`0x10000` in flash, window-relative since [`READ_OFFSET`] is 0x0).
const APP_REGION_OFFSET: usize = 0x10000;
/// `ESP_APP_DESC_MAGIC_WORD` (`0xABCD5432`), little-endian: the first four
/// bytes of every `esp_app_desc_t`, which ESP-IDF mandates in every
/// application image. Conventional spot is 0x10028 (app at 0x10000, image
/// header 0x20, segment header 0x08) but MMU page sizes can shift it, so
/// the whole app region is scanned.
const APP_DESC_MAGIC: [u8; 4] = [0x32, 0x54, 0xCD, 0xAB];

/// Fixed-width, NUL-padded fields inside an `esp_app_desc_t`, at offsets
/// past its magic word: the app's own `version`, then `project_name`,
/// build `time`/`date`, and the ESP-IDF build that produced the image.
const DESC_FIELD_LEN: usize = 32;
const DESC_VERSION_OFFSET: usize = 16;
const DESC_IDF_VERSION_OFFSET: usize = 112;

/// Scans the identification window for firmware signatures. Partition
/// labels decide first --- they are structural, so they cannot appear in
/// a foreign image by accident the way a string can (a
/// MicroPython-on-Zephyr build is labelled Zephyr, which is the honest
/// answer: Zephyr is what manages the board). Banner strings are the
/// fallback, matched case-insensitively across the whole window, since
/// builds differ in casing and in where they keep their banner (Zephyr/
/// MCUboot names itself in the bootloader, MicroPython in the app).
pub fn identify(data: &[u8]) -> Option<FlashFirmware> {
    if zephyr_partition_label(data) {
        return Some(FlashFirmware::Zephyr);
    }
    if contains_ascii_ci(data, b"micropython") {
        return Some(FlashFirmware::MicroPython);
    }
    if contains_ascii_ci(data, b"zephyr") {
        return Some(FlashFirmware::Zephyr);
    }
    if has_esp_idf_app_descriptor(data) {
        return Some(FlashFirmware::EspIdf);
    }
    None
}

/// The version the identified firmware names itself with, read off the
/// same bytes that identified it. MicroPython and Zephyr compile their
/// banners into the image (`MicroPython v1.28.0 on …`, `*** Booting
/// Zephyr OS build v4.0.0 ***`); an ESP-IDF app's descriptor carries
/// stamped fields instead, where the IDF build's version is the
/// deterministic one (an app without a git tag is just `1`).
/// `None` when the window carries no version to read.
pub fn version(data: &[u8], firmware: FlashFirmware) -> Option<String> {
    match firmware {
        FlashFirmware::MicroPython => banner_version(data, b"micropython"),
        FlashFirmware::Zephyr => banner_version(data, b"zephyr os build"),
        FlashFirmware::EspIdf => esp_idf_version(data),
    }
}

/// The identification question the Device info pane actually asks:
/// which firmware the flash carries, or that it carries none. Erased
/// flash is checked first --- every firmware writes into the bootloader
/// region the window starts at, so an all-`0xFF` window can only be a
/// blank chip, never a firmware that happens to be quiet.
pub fn classify(data: &[u8]) -> Option<FirmwareVerdict> {
    if is_erased(data) {
        return Some(FirmwareVerdict::Erased);
    }
    identify(data).map(|kind| FirmwareVerdict::Firmware(kind, version(data, kind)))
}

/// Whether the window reads as erased flash throughout. An empty read is
/// deliberately *not* erased: a failed or truncated `read-flash` must not
/// masquerade as a blank device.
fn is_erased(data: &[u8]) -> bool {
    !data.is_empty() && data.iter().all(|&byte| byte == ERASED)
}

/// Where the app region's `esp_app_desc_t` magic word sits in the
/// window, if it does. Only the app region counts: the ESP-IDF
/// bootloader that fills the region below the partition table is shared
/// by MicroPython and Zephyr builds too, so a magic (or any IDF string)
/// down there says nothing about which firmware is running.
fn esp_idf_app_descriptor(data: &[u8]) -> Option<usize> {
    data[APP_REGION_OFFSET.min(data.len())..]
        .windows(APP_DESC_MAGIC.len())
        .position(|bytes| bytes == APP_DESC_MAGIC)
        .map(|offset| offset + APP_REGION_OFFSET)
}

fn has_esp_idf_app_descriptor(data: &[u8]) -> bool {
    esp_idf_app_descriptor(data).is_some()
}

/// The version fields of the app descriptor the magic word opened: the
/// IDF build's own stamp first, the app's version second (a project
/// without a git tag defaults to a bare `1`, which names nothing).
fn esp_idf_version(data: &[u8]) -> Option<String> {
    let offset = esp_idf_app_descriptor(data)?;
    [DESC_IDF_VERSION_OFFSET, DESC_VERSION_OFFSET]
        .into_iter()
        .find_map(|field| descriptor_field(data, offset + field))
}

/// One NUL-padded fixed-width descriptor field, read as a version: it
/// must be printable, non-empty and carry a digit to count --- anything
/// else is padding or garbage, not a version.
fn descriptor_field(data: &[u8], start: usize) -> Option<String> {
    let field = data.get(start..start.checked_add(DESC_FIELD_LEN)?)?;
    let end = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(DESC_FIELD_LEN);
    let bytes = &field[..end];
    (!bytes.is_empty()
        && bytes.iter().all(|byte| byte.is_ascii_graphic())
        && bytes.iter().any(|byte| byte.is_ascii_digit()))
    .then(|| String::from_utf8_lossy(bytes).into_owned())
}

/// The version token following every case-insensitive occurrence of
/// `marker`, first one that parses (`micropython` also matches paths and
/// build flags, so the first hit may carry no version at all).
fn banner_version(data: &[u8], marker: &[u8]) -> Option<String> {
    data.windows(marker.len())
        .enumerate()
        .filter(|(_, window)| window.eq_ignore_ascii_case(marker))
        .find_map(|(offset, _)| version_token(&data[offset + marker.len()..]))
}

/// The version starting at `rest`: optional spaces, an optional `v`,
/// then the version's own characters. Kept honest by requiring a digit
/// (so `view` or `version` is not a version) and capped at a sane
/// length, since flash bytes around a banner are not trustworthy.
fn version_token(rest: &[u8]) -> Option<String> {
    let mut bytes = rest
        .iter()
        .copied()
        .skip_while(|byte| matches!(byte, b' ' | b'\t'));
    let mut token = Vec::new();
    if matches!(bytes.next(), Some(b'v' | b'V')) {
        token.push(b'v');
    }
    for byte in bytes {
        if !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_')) {
            break;
        }
        token.push(byte);
        if token.len() > DESC_FIELD_LEN {
            return None;
        }
    }
    while token
        .last()
        .is_some_and(|byte| !byte.is_ascii_alphanumeric())
    {
        token.pop();
    }
    (!token.is_empty() && token.iter().any(|byte| byte.is_ascii_digit()))
        .then(|| String::from_utf8_lossy(&token).into_owned())
}

/// Whether any valid partition entry carries a Zephyr label. The table
/// is read at its conventional flash address relative to the window; a
/// window that ends before it (a short read, a different convention)
/// simply has no labels to find.
fn zephyr_partition_label(data: &[u8]) -> bool {
    let table = &data[TABLE_OFFSET_IN_WINDOW.min(data.len())..];
    table
        .chunks_exact(ENTRY_SIZE)
        .take_while(|entry| entry[0] == ENTRY_MAGIC)
        .any(|entry| {
            let label = &entry[LABEL_OFFSET..LABEL_OFFSET + LABEL_LEN];
            let end = label
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(LABEL_LEN);
            ZEPHYR_LABELS
                .iter()
                .any(|candidate| label[..end].eq_ignore_ascii_case(candidate.as_bytes()))
        })
}

/// Case-insensitive ASCII substring search over raw flash bytes.
fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one partition-table entry with `label`, prefixed by the magic
    /// byte, exactly as `gen_esp32part` writes it.
    fn entry(label: &str, kind: u8) -> [u8; ENTRY_SIZE] {
        let mut entry = [0u8; ENTRY_SIZE];
        entry[0] = ENTRY_MAGIC;
        entry[1] = kind;
        entry[4..8].copy_from_slice(&0x10000u32.to_le_bytes());
        entry[8..12].copy_from_slice(&0x100000u32.to_le_bytes());
        let label = label.as_bytes();
        entry[LABEL_OFFSET..LABEL_OFFSET + label.len()].copy_from_slice(label);
        entry
    }

    fn table(entries: &[[u8; ENTRY_SIZE]]) -> Vec<u8> {
        let mut data = Vec::new();
        for chunk in entries {
            data.extend_from_slice(chunk);
        }
        data.extend([0xFF; ENTRY_SIZE]); // the end-of-table marker
        data
    }

    /// A flash window with the partition table at its conventional spot
    /// (0x8000 into the window) and `extra` bytes after it --- the shape
    /// `esptool read-flash 0x0 0x20000` brings back.
    fn window(entries: &[[u8; ENTRY_SIZE]], extra: &[u8]) -> Vec<u8> {
        let mut data = vec![0xFF; TABLE_OFFSET_IN_WINDOW];
        data.extend(table(entries));
        data.extend_from_slice(extra);
        data
    }

    #[test]
    fn mcuboot_label_identifies_zephyr() {
        let data = window(
            &[
                entry("nvs", 1),
                entry("mcuboot", 0),
                entry("slot0_partition", 0),
            ],
            b"",
        );
        assert_eq!(identify(&data), Some(FlashFirmware::Zephyr));
    }

    #[test]
    fn micropython_banner_string_identifies_micropython() {
        let data = window(
            &[entry("nvs", 1), entry("factory", 0)],
            b"MicroPython v1.25.0 on 2025-01-01; ESP32 module\n",
        );
        assert_eq!(identify(&data), Some(FlashFirmware::MicroPython));
    }

    #[test]
    fn zephyr_boot_string_identifies_zephyr_without_bootloader_labels() {
        // A Zephyr app booted by some other bootloader still names itself;
        // on a real sysbuild image that banner lives in the bootloader
        // region below the partition table (verified on hardware).
        let mut data = vec![0xFF; 0x1000];
        data.extend_from_slice(b"*** Booting Zephyr OS build v4.0.0-***\n");
        assert_eq!(identify(&data), Some(FlashFirmware::Zephyr));
    }

    #[test]
    fn micropython_string_wins_when_labels_say_nothing() {
        // Stock MicroPython partition labels are generic ("factory"),
        // so the string is what must answer.
        let mut data = window(&[entry("nvs", 1), entry("factory", 0)], b"");
        assert_eq!(identify(&data), None, "no strings, generic labels");
        data.extend_from_slice(b"micropython build");
        assert_eq!(identify(&data), Some(FlashFirmware::MicroPython));
    }

    #[test]
    fn zephyr_label_outranks_a_micropython_string() {
        // MicroPython running *on* Zephyr: the partition layout is the
        // structural truth, so Zephyr wins.
        let data = window(
            &[entry("mcuboot", 0), entry("slot0_partition", 0)],
            b"MicroPython v1.25.0 on zephyr",
        );
        assert_eq!(identify(&data), Some(FlashFirmware::Zephyr));
    }

    #[test]
    fn bootloader_only_string_identifies_zephyr() {
        // The hardware case that shaped the read window: the banner sits
        // in the bootloader region and the app area says nothing.
        let mut data = vec![0xFF; TABLE_OFFSET_IN_WINDOW];
        data[..b"ZEPHYR".len()].copy_from_slice(b"ZEPHYR");
        assert_eq!(identify(&data), Some(FlashFirmware::Zephyr));
    }

    #[test]
    fn erased_flash_identifies_nothing() {
        assert_eq!(identify(&[0xFF; READ_SIZE]), None);
    }

    #[test]
    fn erased_flash_classifies_as_no_firmware() {
        assert_eq!(classify(&[0xFF; READ_SIZE]), Some(FirmwareVerdict::Erased));
    }

    #[test]
    fn an_empty_read_is_not_a_blank_device() {
        // A failed or truncated read must not read as "no firmware":
        // that verdict claims the chip is blank.
        assert_eq!(classify(&[]), None);
    }

    #[test]
    fn unrecognized_contents_stay_unrecognized() {
        // Zeros are written bytes without any signature --- neither a
        // firmware nor an erased chip.
        assert_eq!(classify(&[0x00; READ_SIZE]), None);
    }

    #[test]
    fn a_named_firmware_classifies_as_itself() {
        let data = window(&[entry("nvs", 1), entry("factory", 0)], b"MicroPython v1");
        assert_eq!(
            classify(&data),
            Some(FirmwareVerdict::Firmware(
                FlashFirmware::MicroPython,
                Some("v1".to_string())
            ))
        );
    }

    #[test]
    fn the_banner_names_the_micropython_version() {
        let data = window(
            &[entry("nvs", 1), entry("factory", 0)],
            b"\x00MicroPython v1.28.0 on 2025-11-01; ESP32 module\n",
        );
        assert_eq!(
            version(&data, FlashFirmware::MicroPython),
            Some("v1.28.0".to_string())
        );
    }

    #[test]
    fn a_micropython_daily_build_keeps_its_full_version() {
        let data = window(
            &[entry("nvs", 1), entry("factory", 0)],
            b"MicroPython v1.25.0-123.g0123abcdef on 2025-01-01\n",
        );
        assert_eq!(
            version(&data, FlashFirmware::MicroPython),
            Some("v1.25.0-123.g0123abcdef".to_string())
        );
    }

    #[test]
    fn a_zephyr_banner_names_its_build_version() {
        let mut data = vec![0xFF; 0x1000];
        data.extend_from_slice(b"*** Booting Zephyr OS build v4.0.0-***\n");
        assert_eq!(
            version(&data, FlashFirmware::Zephyr),
            Some("v4.0.0".to_string()),
            "the trailing dash before the asterisks is not part of the version"
        );
    }

    #[test]
    fn a_zephyr_git_describe_banner_keeps_its_full_version() {
        // The banner as a simple-boot board actually prints it (captured on
        // hardware): a git-describe string, not a bare tag, sitting deep
        // past the identification window.
        let mut data = vec![0xFF; HUNT_OFFSET];
        data.extend_from_slice(b"*** Booting Zephyr OS build v4.4.0-11847-gc5dffcb7c9da ***\n");

        // The identification window: names the firmware (the kernel's
        // strings sit early), but cannot date it.
        let marker = b">>> ZEPHYR FATAL ERROR %d: %s on CPU %d";
        data[..marker.len()].copy_from_slice(marker);
        assert_eq!(
            classify(&data[..READ_SIZE]),
            Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None))
        );

        // The hunt window: the same bytes' follow-up region dates the
        // verdict the first window left bare.
        assert_eq!(
            version(&data[HUNT_OFFSET..], FlashFirmware::Zephyr),
            Some("v4.4.0-11847-gc5dffcb7c9da".to_string())
        );
    }

    #[test]
    fn a_graphics_heavy_simple_boot_banner_still_falls_inside_the_hunt() {
        // Captured from a real esp32c3-round-display build (LVGL over a
        // round SPI panel): `zephyr.bin` past 1 MiB pushed the banner to
        // byte offset 0xd06a8 --- past the original 512 KiB hunt budget,
        // which is exactly what widened it to 1 MiB.
        let banner_offset = 0xd06a8 - HUNT_OFFSET;
        let banner = b"*** Booting Zephyr OS build v4.4.0 ***\n";
        let mut data = vec![0xFF; HUNT_SIZE];
        data[banner_offset..banner_offset + banner.len()].copy_from_slice(banner);
        assert_eq!(
            version(&data, FlashFirmware::Zephyr),
            Some("v4.4.0".to_string()),
            "the wider hunt window must still reach a banner this deep"
        );
    }

    #[test]
    fn labels_without_a_banner_carry_no_version() {
        // A Zephyr sysbuild image whose banner sits outside the window:
        // the partition labels still identify, but no version may be
        // invented for them.
        let data = window(&[entry("mcuboot", 0), entry("slot0_partition", 0)], b"");
        assert_eq!(
            classify(&data),
            Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None))
        );
    }

    #[test]
    fn a_non_version_word_after_the_banner_is_not_a_version() {
        let data = window(
            &[entry("nvs", 1), entry("factory", 0)],
            b"micropython build",
        );
        assert_eq!(version(&data, FlashFirmware::MicroPython), None);
    }

    #[test]
    fn a_later_banner_still_names_the_version() {
        // The first `micropython` hit may be a path fragment with no
        // version after it; the scan owes the answer to the real banner.
        let mut data = window(&[entry("nvs", 1), entry("factory", 0)], b"");
        data.extend_from_slice(b"/build/micropython/port\x00");
        data.extend_from_slice(b"MicroPython v1.28.0 on 2025-11-01\n");
        assert_eq!(
            version(&data, FlashFirmware::MicroPython),
            Some("v1.28.0".to_string())
        );
    }

    #[test]
    fn esp_idf_only_output_identifies_nothing() {
        // Banner strings alone classify nothing: the ESP-IDF bootloader's
        // `/IDF/components/...` paths show up in MicroPython and Zephyr
        // builds too (both build on that bootloader), so without the app
        // descriptor there is no honest ESP-IDF verdict.
        let data = window(
            &[entry("nvs", 1), entry("factory", 0)],
            b"ESP-IDF v5.1.4 hello_world /IDF/components/bootloader_support",
        );
        assert_eq!(identify(&data), None);
    }

    /// A window with the app descriptor magic at its conventional spot
    /// (0x10028: app at 0x10000, image header 0x20, segment header 0x08).
    fn espidf_window() -> Vec<u8> {
        let mut data = window(&[entry("nvs", 1), entry("factory", 0)], b"");
        data.resize(APP_REGION_OFFSET + 0x28, 0xFF);
        data.extend_from_slice(&APP_DESC_MAGIC);
        data
    }

    /// The full descriptor past its magic word: the reserved words, the
    /// app's `version`, `project_name`, build `time`/`date`, then the
    /// IDF build's version --- every field NUL-padded to 32 bytes, the
    /// shape `esp_app_desc_t` mandates.
    fn descriptor(data: &mut Vec<u8>, offset: usize, field: usize, value: &[u8]) {
        let start = offset + field;
        data.resize(start, 0);
        data.extend_from_slice(value);
        data.resize(start + DESC_FIELD_LEN, 0);
    }

    #[test]
    fn esp_idf_app_descriptor_identifies_espidf() {
        let data = espidf_window();
        assert_eq!(identify(&data), Some(FlashFirmware::EspIdf));
        assert_eq!(version(&data, FlashFirmware::EspIdf), None);
    }

    #[test]
    fn the_app_descriptor_names_the_idf_version() {
        let mut data = espidf_window();
        let offset = data.len() - APP_DESC_MAGIC.len();
        descriptor(&mut data, offset, DESC_VERSION_OFFSET, b"1");
        descriptor(&mut data, offset, DESC_IDF_VERSION_OFFSET, b"v5.3.1");
        assert_eq!(
            version(&data, FlashFirmware::EspIdf),
            Some("v5.3.1".to_string()),
            "the IDF build's stamp outranks an app version that is just `1`"
        );
    }

    #[test]
    fn a_meaningful_app_version_is_the_espidf_fallback() {
        let mut data = espidf_window();
        let offset = data.len() - APP_DESC_MAGIC.len();
        descriptor(&mut data, offset, DESC_VERSION_OFFSET, b"v2.4.1");
        assert_eq!(
            version(&data, FlashFirmware::EspIdf),
            Some("v2.4.1".to_string())
        );
    }

    #[test]
    fn esp_idf_magic_below_the_app_region_stays_undefined() {
        // The bootloader is shared by all three firmwares, so an IDF-shaped
        // byte sequence in the bootloader region must not classify --- only
        // the app region answers "what is running".
        let mut data = vec![0xFF; 0x1000];
        data[..APP_DESC_MAGIC.len()].copy_from_slice(&APP_DESC_MAGIC);
        assert_eq!(identify(&data), None);
    }

    #[test]
    fn named_firmwares_outrank_the_esp_idf_magic() {
        // A MicroPython or Zephyr app also embeds an esp_app_desc (both
        // build on ESP-IDF), so the descriptor only answers once the
        // name-bearing signatures have had their say.
        let mut micropython = espidf_window();
        micropython.extend_from_slice(b"MicroPython v1.25.0");
        assert_eq!(identify(&micropython), Some(FlashFirmware::MicroPython));

        let mut zephyr = espidf_window();
        zephyr.extend_from_slice(b"*** Booting Zephyr OS ***");
        assert_eq!(identify(&zephyr), Some(FlashFirmware::Zephyr));
    }
}
