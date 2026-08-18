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
//! no firmware on it at all.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareVerdict {
    Firmware(FlashFirmware),
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
/// and banner strings of either firmware wherever it keeps them; far
/// cheaper than reading the whole chip.
pub const READ_SIZE: usize = 0x20000;

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

/// The identification question the Device info pane actually asks:
/// which firmware the flash carries, or that it carries none. Erased
/// flash is checked first --- every firmware writes into the bootloader
/// region the window starts at, so an all-`0xFF` window can only be a
/// blank chip, never a firmware that happens to be quiet.
pub fn classify(data: &[u8]) -> Option<FirmwareVerdict> {
    if is_erased(data) {
        return Some(FirmwareVerdict::Erased);
    }
    identify(data).map(FirmwareVerdict::Firmware)
}

/// Whether the window reads as erased flash throughout. An empty read is
/// deliberately *not* erased: a failed or truncated `read-flash` must not
/// masquerade as a blank device.
fn is_erased(data: &[u8]) -> bool {
    !data.is_empty() && data.iter().all(|&byte| byte == ERASED)
}

/// Whether the app region carries an `esp_app_desc_t` magic word. Only the
/// app region counts: the ESP-IDF bootloader that fills the region below
/// the partition table is shared by MicroPython and Zephyr builds too, so
/// a magic (or any IDF string) down there says nothing about which
/// firmware is running --- the check runs after the two name-bearing
/// firmwares have had their say for the same reason.
fn has_esp_idf_app_descriptor(data: &[u8]) -> bool {
    let app = &data[APP_REGION_OFFSET.min(data.len())..];
    app.windows(APP_DESC_MAGIC.len())
        .any(|bytes| bytes == APP_DESC_MAGIC)
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
            Some(FirmwareVerdict::Firmware(FlashFirmware::MicroPython))
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

    #[test]
    fn esp_idf_app_descriptor_identifies_espidf() {
        let data = espidf_window();
        assert_eq!(identify(&data), Some(FlashFirmware::EspIdf));
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
