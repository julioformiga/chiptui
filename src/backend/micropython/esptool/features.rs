//! Compaction of esptool's `Features:` line for the Device info pane.
//!
//! The pane is a fixed four rows and the features line is one of them, so at
//! the declared minimum width ([`crate::ui::MIN_WIDTH`]) the value has 27
//! columns. esptool's own list does not remotely fit --- a plain ESP32
//! reports (verbatim, from `esptool/targets/esp32.py`'s `get_chip_features`):
//!
//! ```text
//! Wi-Fi, BT, Dual Core + LP Core, 240MHz, Coding Scheme None
//! ```
//!
//! Truncating that kept the head and threw away the rest, which on an ESP32-S3
//! (`Wi-Fi, BT 5 (LE), Dual Core + LP Core, 240MHz, Embedded Flash 8MB (XMC),
//! Embedded PSRAM 8MB (AP_3v3)`) swallowed the PSRAM. So the list is
//! re-expressed instead: `WiFi, BLE5, 2x240MHz, 8MB`. The shortening is
//! purely verbal --- no glyphs. An icon per feature was tried and reverted: a
//! symbol standing in for `WiFi` costs the reader more than the three columns
//! it saves, and there is no non-PUA Unicode character for Bluetooth at all.
//! Words this short are already the compact form.
//!
//! Everything the output generates is ASCII (see `only_ascii_is_ever_
//! generated`), which is what makes the pane's `chars()`-based width budget
//! exact --- this crate has no `unicode-width` to consult.
//!
//! Nothing is deleted by decree: an unrecognised token rides the tail verbatim
//! and the raw line still reaches the Log pane
//! (`crate::flash::FlashPanel::complete`), the same pairing the `Firmware` row
//! already uses with `short_version`.
//!
//! Tolerant by construction, same spirit as [`super::parse`]: matching is
//! case-insensitive, because esptool's wording varies by chip and has
//! drifted across major versions --- and every shape quoted in this module's
//! doc comments and tests was copied from an installed esptool v5.3.1 (the
//! version this crate's fixtures are pinned to, `AGENTS.md`'s "read the
//! tool's source before writing the fake" rule applied for real), not
//! invented. Notably esptool spells it **`Wi-Fi`**, hyphenated, always; a
//! bluetooth radio's version and its Low Energy support arrive as *one*
//! already-combined token (`"BT 5 (LE)"`), never as separate `BT`/`BLE`
//! entries; and a core count often carries a `+ LP Core` suffix this module
//! deliberately drops --- `crate::flash::FlashAction`'s reader wants "how
//! many cores, how fast", not whether one of them is a low-power coprocessor.

/// One compacted entry of the features line, in the order the pane wants them.
///
/// `muted` marks a token this module did not recognise and is passing through
/// verbatim: the pane renders those in the muted colour and drops them first
/// when the row runs out of room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub muted: bool,
}

impl Item {
    fn known(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            muted: false,
        }
    }

    fn passthrough(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            muted: true,
        }
    }

    /// Columns this entry occupies.
    pub fn width(&self) -> usize {
        self.text.chars().count()
    }
}

/// What esptool said about Bluetooth. Real esptool never splits this across
/// tokens --- one comma-separated entry carries the version and the `(LE)`
/// flag together (`"BT 5 (LE)"`), or is the bare classic-only `"BT"` a plain
/// ESP32 reports --- so this only ever gets filled from a single token, not
/// merged across several.
#[derive(Default)]
struct Bluetooth {
    present: bool,
    /// `None` for the bare `"BT"` esptool prints when a chip has no LE radio
    /// (a plain ESP32) --- there is nothing to pair the plain label with.
    version: Option<String>,
    low_energy: bool,
    /// `"BT 5.4 (LE) + Classic"`-shaped tokens (dual-mode radios): kept
    /// separately from `low_energy` so a chip that is LE-only never claims
    /// classic support it does not have.
    classic: bool,
}

impl Bluetooth {
    fn label(&self) -> Option<String> {
        if !self.present {
            return None;
        }
        let prefix = if self.low_energy { "BLE" } else { "BT" };
        let version = self.version.as_deref().unwrap_or("");
        let mut label = format!("{prefix}{version}");
        if self.classic {
            label.push_str("+BT");
        }
        Some(label)
    }
}

/// Re-expresses esptool's comma-separated feature list, most identifying
/// first: radios, then cores/clock, then embedded memory, then whatever was
/// not recognised.
///
/// The order *is* the priority: the pane fills its single row from the front
/// and drops whole entries off the back, so the tail is what a narrow terminal
/// loses.
pub fn compact(raw: &str) -> Vec<Item> {
    let mut wifi = None;
    let mut bluetooth = Bluetooth::default();
    let mut mesh = false;
    let mut memory = Vec::new();
    let mut rest = Vec::new();
    let mut cores: Option<u32> = None;
    let mut clock: Option<String> = None;

    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let lower = token.to_ascii_lowercase();

        // `Wi-Fi`, `Wi-Fi 6`, `Wi-Fi 6E (tri-band)`. The generation is a
        // leading digit, optionally one trailing `e`; any further
        // parenthetical detail is dropped. (Splitting `raw` on every comma
        // is what the rest of this module relies on too, so a generation
        // note with a comma *inside* its parens --- `esp32e22.py`'s `"Wi-Fi
        // 6E (tri-band, 2x2 MU-MIMO)"`, not a chip `ChipFamily` offers ---
        // would itself be split in two; not worth a parser rewrite for a
        // board this crate cannot select.) A suffix that does not start
        // with a digit is something this module does not understand
        // (esptool has never emitted one for a supported chip, but nothing
        // guarantees it never will), so the token falls through to the tail
        // whole rather than being reported as plain WiFi.
        if let Some(after) = lower
            .strip_prefix("wi-fi")
            .or_else(|| lower.strip_prefix("wifi"))
        {
            let after = after.trim();
            if after.is_empty() {
                wifi = Some("WiFi".to_string());
                continue;
            }
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                let mut generation = digits.clone();
                if after[digits.len()..].starts_with('e') {
                    generation.push('E');
                }
                wifi = Some(format!("WiFi{generation}"));
                continue;
            }
        }

        // `BT`, `BT 5 (LE)`, `BT 5.4 (LE) + Classic` --- see [`Bluetooth`].
        if let Some(after) = lower.strip_prefix("bt") {
            let after = after.trim();
            if after.is_empty() {
                bluetooth.present = true;
                continue;
            }
            if after.starts_with(|c: char| c.is_ascii_digit()) {
                let version_len = after
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .count();
                let (version, tail) = after.split_at(version_len);
                bluetooth.present = true;
                bluetooth.version = Some(version.to_string());
                bluetooth.low_energy = tail.contains("(le)");
                bluetooth.classic = tail.contains("classic");
                continue;
            }
        }
        // A hypothetical bare `BLE` token: never seen from real esptool
        // (which always folds the LE flag into the `BT …` token above), but
        // cheap to tolerate.
        if lower == "ble" {
            bluetooth.present = true;
            bluetooth.low_energy = true;
            continue;
        }

        // Thread/Zigbee's radio. Anyone who knows 802.15.4 reads `15.4`.
        if lower.starts_with("ieee802.15.4") || lower.starts_with("802.15.4") {
            mesh = true;
            continue;
        }

        // `Single Core`, `Dual Core`, and either with a `+ LP Core` suffix
        // (ESP32/S3/C6 pair every core count with their low-power
        // coprocessor). The suffix is dropped: this row answers "how many
        // cores, how fast", not which of them is the LP one.
        if lower.starts_with("dual core") {
            cores = Some(2);
            continue;
        }
        if lower.starts_with("single core") {
            cores = Some(1);
            continue;
        }

        // `240MHz`. Kept in the token's own casing --- this is a value, not a
        // label that needs normalising.
        if let Some(head) = lower.strip_suffix("mhz")
            && !head.is_empty()
            && head.chars().all(|c| c.is_ascii_digit())
        {
            clock = Some(token.to_string());
            continue;
        }

        // Embedded flash is the unremarkable one, so its size stands alone;
        // PSRAM has to say its name or `4MB, 2MB` would be a riddle. No space
        // between name and size, which is what keeps an ESP32-S3's full
        // report (`WiFi, BLE5, 2x240MHz, 8MB, PSRAM8MB`) inside the 27
        // columns the row has at the minimum width. Only the two forms that
        // start with the size question ("Embedded Flash …") are recognised
        // here --- esptool's `"No Embedded Flash"`/`"Unknown Embedded
        // Flash"` (an ESP32-S2 with nothing to report) start with the
        // adjective instead and fall through to the tail unclaimed, which is
        // the right place for a fact that says nothing was found.
        if let Some(size) = embedded_size(&lower, "embedded flash") {
            memory.push(Item::known(if size.is_empty() {
                "flash".to_string()
            } else {
                size
            }));
            continue;
        }
        if let Some(size) = embedded_size(&lower, "embedded psram") {
            memory.push(Item::known(format!("PSRAM{size}")));
            continue;
        }

        rest.push(Item::passthrough(token));
    }

    let mut items: Vec<Item> = wifi
        .map(Item::known)
        .into_iter()
        .chain(bluetooth.label().map(Item::known))
        .chain(mesh.then(|| Item::known("15.4")))
        .collect();
    // Cores and clock describe one thing --- how fast this part computes ---
    // and cost 19 characters apart (`Dual Core`, `240MHz` and two separators)
    // against 8 together.
    match (cores, clock) {
        (Some(cores), Some(clock)) => items.push(Item::known(format!("{cores}x{clock}"))),
        (None, Some(clock)) => items.push(Item::known(clock)),
        (Some(1), None) => items.push(Item::known("1 core")),
        (Some(cores), None) => items.push(Item::known(format!("{cores} cores"))),
        (None, None) => {}
    }
    items.extend(memory);
    items.extend(rest);
    items
}

/// `"embedded flash 4mb (xmc)"` -> `"4MB"`, the vendor dropped. `None` when
/// `lower` is not that kind of token at all; `Some("")` when it is one with
/// no size attached (a plain ESP32/ESP8266 says just `Embedded Flash`).
fn embedded_size(lower: &str, prefix: &str) -> Option<String> {
    let tail = lower.strip_prefix(prefix)?.trim();
    let Some(word) = tail.split_whitespace().next() else {
        return Some(String::new());
    };
    let is_size = word
        .strip_suffix("mb")
        .or_else(|| word.strip_suffix("kb"))
        .or_else(|| word.strip_suffix("gb"))
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()));
    Some(if is_size {
        word.to_ascii_uppercase()
    } else {
        String::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `esptool` v5.3.1 output, one per `ChipFamily` this crate
    /// supports (`super::ChipFamily::ALL`), copied verbatim from
    /// `esptool/targets/{esp32,esp32s2,esp32s3,esp32c3,esp32c6,esp8266}.py`'s
    /// `get_chip_features` --- not invented (`AGENTS.md`'s "reproduce the
    /// tool, not the belief about it").
    const ESP32: &str = "Wi-Fi, BT, Dual Core + LP Core, 240MHz, Coding Scheme None";
    const ESP32_S2: &str = "Wi-Fi, Single Core, 240MHz, Embedded Flash 4MB, \
                             Embedded PSRAM 2MB, No calibration in BLK2 of efuse";
    const ESP32_S3: &str = "Wi-Fi, BT 5 (LE), Dual Core + LP Core, 240MHz, \
                             Embedded Flash 8MB (XMC), Embedded PSRAM 8MB (AP_3v3)";
    const ESP32_C3: &str = "Wi-Fi, BT 5 (LE), Single Core, 160MHz, Embedded Flash 4MB (XMC)";
    const ESP32_C6: &str = "Wi-Fi 6, BT 5 (LE), IEEE802.15.4, Single Core + LP Core, \
                             160MHz, Embedded Flash 8MB";
    const ESP8266: &str = "Wi-Fi, 160MHz, Embedded Flash";

    /// The rendered row, separators included, as the pane assembles it --- so
    /// the expectations below read as what the user sees.
    fn rendered(raw: &str) -> String {
        compact(raw)
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[test]
    fn only_ascii_is_ever_generated() {
        // The pane budgets its fixed-height rows by `chars()` and this crate
        // has no `unicode-width`: as long as what this module builds is
        // ASCII, that count is the column count exactly. (A passed-through
        // token is esptool's text, not this module's, and is exempt.)
        for banner in [ESP32, ESP32_S2, ESP32_S3, ESP32_C3, ESP32_C6, ESP8266] {
            for item in compact(banner).iter().filter(|item| !item.muted) {
                assert!(
                    item.text.is_ascii(),
                    "`{}` is not ASCII, so the row's width budget is a guess",
                    item.text
                );
            }
        }
    }

    #[test]
    fn an_esp32_loses_only_the_coding_scheme_trivia_from_its_head() {
        assert_eq!(rendered(ESP32), "WiFi, BT, 2x240MHz, Coding Scheme None");
        // The three identifying facts fit the 27 columns the row has at the
        // declared minimum width.
        let head = "WiFi, BT, 2x240MHz";
        assert!(head.chars().count() <= 27, "{head}");
    }

    #[test]
    fn an_esp32_c3_matches_a_real_board_exactly() {
        // This is the exact case a real board reported out of order (radios
        // and cores after the memory fact) before esptool's actual token
        // shapes --- hyphenated `Wi-Fi`, one merged `BT 5 (LE)` token, no
        // separate `BLE` --- were recognised.
        assert_eq!(rendered(ESP32_C3), "WiFi, BLE5, 1x160MHz, 4MB");
    }

    #[test]
    fn an_esp32_s3_keeps_the_psram_the_old_truncation_used_to_swallow() {
        assert_eq!(rendered(ESP32_S3), "WiFi, BLE5, 2x240MHz, 8MB, PSRAM8MB");
        // At the declared minimum width the row has 27 columns: everything
        // but the PSRAM fact fits, so that is what drops off the tail ---
        // never a `…` standing in for the rest.
        let head = "WiFi, BLE5, 2x240MHz, 8MB";
        assert!(head.chars().count() <= 27, "{head}");
        assert!(rendered(ESP32_S3).chars().count() > 27);
    }

    #[test]
    fn an_esp32_c6_carries_the_wifi_generation_the_mesh_radio_and_bluetooth() {
        assert_eq!(rendered(ESP32_C6), "WiFi6, BLE5, 15.4, 1x160MHz, 8MB");
    }

    #[test]
    fn an_esp32_s2_names_the_psram_so_two_sizes_are_not_a_riddle() {
        assert_eq!(
            rendered(ESP32_S2),
            "WiFi, 1x240MHz, 4MB, PSRAM2MB, No calibration in BLK2 of efuse"
        );
    }

    #[test]
    fn an_esp8266_states_embedded_flash_without_a_size() {
        assert_eq!(rendered(ESP8266), "WiFi, 160MHz, flash");
    }

    #[test]
    fn a_wifi_generation_letter_suffix_is_kept_digits_only_the_rest_dropped() {
        assert_eq!(rendered("Wi-Fi 6E (tri-band)"), "WiFi6E");
        assert_eq!(rendered("Wi-Fi 6 (dual-band)"), "WiFi6");
    }

    #[test]
    fn a_dual_mode_bluetooth_radio_keeps_both_facts_in_one_entry() {
        // Not a chip this crate offers today, but esptool's own shape for
        // one that speaks both classic and LE Bluetooth.
        assert_eq!(rendered("BT 5.4 (LE) + Classic"), "BLE5.4+BT");
    }

    #[test]
    fn a_bare_ble_token_is_tolerated_even_though_esptool_never_sends_one() {
        assert_eq!(rendered("BLE"), "BLE");
    }

    #[test]
    fn an_unrecognised_token_rides_the_tail_verbatim() {
        // What a future esptool might add: kept, muted, last --- so a narrow
        // row loses it before it loses the radios.
        assert_eq!(
            compact("Wi-Fi, Quantum Coprocessor, BT 5 (LE)"),
            vec![
                Item::known("WiFi"),
                Item::known("BLE5"),
                Item::passthrough("Quantum Coprocessor"),
            ]
        );
    }

    #[test]
    fn a_wifi_or_bt_token_with_an_unexpected_suffix_is_not_reported_as_plain() {
        // Better to show the whole odd token than to claim it says `WiFi`.
        assert_eq!(rendered("WiFi HaLow"), "WiFi HaLow");
        assert_eq!(rendered("BT/Ethernet"), "BT/Ethernet");
    }

    #[test]
    fn nothing_recognisable_yields_nothing() {
        assert!(compact("").is_empty());
        assert!(compact("  ,  ,").is_empty());
    }
}
