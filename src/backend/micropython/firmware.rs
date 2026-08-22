//! Firmware discovery on micropython.org/download/.
//!
//! `SPEC.md` §9/§23: the download site has no JSON API, only HTML, so this
//! module owns every bit of scraping --- tolerant substring parsing in the
//! same spirit as `esptool::parse`/`micropython::parse`, not a full HTML
//! parser, matched against the site's stable, simple markup (verified
//! against the live site during implementation; snapshots live in
//! `tests/fixtures/html/`). A parse failure here must read as "found
//! nothing", never a crash --- the caller always allows a pasted direct URL
//! as a fallback.

const BASE_URL: &str = "https://micropython.org";

/// One board returned by a filtered board-list page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardCandidate {
    /// The site's board id, e.g. `ESP32_GENERIC` --- also its download path.
    pub id: String,
    pub product: String,
    pub vendor: String,
}

/// One downloadable file offered on a board page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareFile {
    /// The link text as shown on the page, e.g. `"v1.28.0 (2026-04-06) .bin"`.
    pub label: String,
    /// Empty for `text` before parsing failed/did not apply.
    pub version: String,
    pub date: String,
    /// The heading this file was listed under, empty for the board's
    /// default variant (e.g. `"ESP32 D2WD"`, `"Support for SPIRAM / WROVER"`).
    pub variant: String,
    pub url: String,
    pub kind: FirmwareKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareKind {
    /// A full, flashable image --- the only kind offered as a candidate to
    /// download-and-flash (`SPEC.md` §9).
    Bin,
    /// A partial OTA update image; not something `esptool write-flash
    /// <offset>` should receive.
    AppBin,
    Elf,
}

impl FirmwareKind {
    fn from_path(path: &str) -> Option<Self> {
        if path.ends_with(".app-bin") {
            Some(Self::AppBin)
        } else if path.ends_with(".bin") {
            Some(Self::Bin)
        } else if path.ends_with(".elf") {
            Some(Self::Elf)
        } else {
            None
        }
    }
}

/// The URL for a board-list search, narrowed by MCU alone
/// (`ChipFamily::micropython_mcu_filter`). The vendor is not a query
/// filter anymore: without it the page lists every board for the MCU, and
/// the selection table's `Vendor` column is what tells them apart.
pub fn board_list_url(mcu: &str) -> String {
    format!("{BASE_URL}/download/?mcu={mcu}")
}

/// The URL for one board's firmware page.
pub fn board_page_url(board_id: &str) -> String {
    format!("{BASE_URL}/download/{board_id}/")
}

/// Reads every `<a class="board-card" href="ID">...<div class="board-product">
/// PRODUCT</div>...<div class="board-vendor">VENDOR</div>...</a>` block.
pub fn parse_board_list(html: &str) -> Vec<BoardCandidate> {
    const MARKER: &str = "<a class=\"board-card\" href=\"";
    let mut boards = Vec::new();
    let mut rest = html;

    while let Some(start) = rest.find(MARKER) {
        rest = &rest[start + MARKER.len()..];
        let Some(end) = rest.find('"') else { break };
        let id = rest[..end].to_string();

        let (Some(product), Some(vendor)) = (
            extract_div_text(rest, "board-product"),
            extract_div_text(rest, "board-vendor"),
        ) else {
            continue;
        };

        boards.push(BoardCandidate {
            id,
            product,
            vendor,
        });
    }

    boards
}

fn extract_div_text(html: &str, class: &str) -> Option<String> {
    let marker = format!("<div class=\"{class}\">");
    let start = html.find(&marker)? + marker.len();
    let end = start + html[start..].find("</div>")?;
    Some(html[start..end].trim().to_string())
}

/// Reads every firmware link on a board page, grouped by the `<h2>Firmware
/// [(VARIANT)]</h2>` heading it appears under.
pub fn parse_firmware_files(html: &str) -> Vec<FirmwareFile> {
    const HEADING: &str = "<h2>Firmware";
    let mut files = Vec::new();
    let mut cursor = 0usize;

    while let Some(rel) = html[cursor..].find(HEADING) {
        let start = cursor + rel;
        let Some(heading_end) = html[start..]
            .find("</h2>")
            .map(|e| start + e + "</h2>".len())
        else {
            break;
        };
        let variant = extract_variant(&html[start..heading_end]);

        let next = html[heading_end..]
            .find(HEADING)
            .map_or(html.len(), |rel| heading_end + rel);
        files.extend(parse_links_in_section(&html[heading_end..next], &variant));
        cursor = next;
    }

    files
}

fn extract_variant(heading: &str) -> String {
    let inner = heading
        .trim_start_matches("<h2>Firmware")
        .trim_end_matches("</h2>")
        .trim();
    inner
        .trim_start_matches('(')
        .trim_end_matches(')')
        .to_string()
}

fn parse_links_in_section(section: &str, variant: &str) -> Vec<FirmwareFile> {
    const MARKER: &str = "href=\"/resources/firmware/";
    let mut files = Vec::new();
    let mut rest = section;

    while let Some(start) = rest.find(MARKER) {
        rest = &rest[start + "href=\"".len()..];
        let Some(end_quote) = rest.find('"') else {
            break;
        };
        let path = rest[..end_quote].to_string();
        rest = &rest[end_quote + 1..];

        let Some(gt) = rest.find('>') else { break };
        rest = &rest[gt + 1..];
        let Some(close) = rest.find("</a>") else {
            break;
        };
        let label = rest[..close].trim().to_string();
        rest = &rest[close..];

        let Some(kind) = FirmwareKind::from_path(&path) else {
            continue;
        };
        let (version, date) = if kind == FirmwareKind::Bin {
            parse_bin_label(&label)
        } else {
            (String::new(), String::new())
        };

        files.push(FirmwareFile {
            label,
            version,
            date,
            variant: variant.to_string(),
            url: format!("{BASE_URL}{path}"),
            kind,
        });
    }

    files
}

/// `"v1.28.0 (2026-04-06) .bin"` -> `("v1.28.0", "2026-04-06")`. Also
/// matches preview builds, e.g. `"v1.29.0-preview.678.g5f2181f938
/// (2026-08-07) .bin"`.
fn parse_bin_label(label: &str) -> (String, String) {
    let version = label.split_whitespace().next().unwrap_or("").to_string();
    let date = label
        .find('(')
        .and_then(|open| {
            label[open + 1..]
                .find(')')
                .map(|close| label[open + 1..open + 1 + close].to_string())
        })
        .unwrap_or_default();
    (version, date)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOWNLOAD_ESP32: &str =
        include_str!("../../../tests/fixtures/html/micropython_download_esp32.html");
    const BOARD_ESP32_GENERIC: &str =
        include_str!("../../../tests/fixtures/html/micropython_board_esp32_generic.html");

    #[test]
    fn board_list_url_narrows_by_mcu_alone() {
        assert_eq!(
            board_list_url("esp32"),
            "https://micropython.org/download/?mcu=esp32"
        );
    }

    #[test]
    fn board_page_url_matches_the_sites_path_shape() {
        assert_eq!(
            board_page_url("ESP32_GENERIC"),
            "https://micropython.org/download/ESP32_GENERIC/"
        );
    }

    #[test]
    fn parses_every_board_on_an_unfiltered_mcu_search() {
        let boards = parse_board_list(DOWNLOAD_ESP32);
        assert_eq!(boards.len(), 9, "boards found: {boards:?}");
        assert!(boards.contains(&BoardCandidate {
            id: "ESP32_GENERIC".to_string(),
            product: "ESP32 / WROOM".to_string(),
            vendor: "Espressif".to_string(),
        }));
        assert!(boards.iter().any(|b| b.vendor == "Olimex"));
    }

    #[test]
    fn a_page_with_no_boards_yields_an_empty_list_not_an_error() {
        assert!(parse_board_list("<html><body>nothing here</body></html>").is_empty());
    }

    #[test]
    fn parses_the_generic_variants_bin_release() {
        let files = parse_firmware_files(BOARD_ESP32_GENERIC);
        let latest = files
            .iter()
            .find(|f| f.kind == FirmwareKind::Bin && f.version == "v1.28.0" && f.variant.is_empty())
            .unwrap_or_else(|| panic!("v1.28.0 .bin not found in {files:?}"));

        assert_eq!(latest.date, "2026-04-06");
        assert_eq!(
            latest.url,
            "https://micropython.org/resources/firmware/ESP32_GENERIC-20260406-v1.28.0.bin"
        );
    }

    #[test]
    fn groups_files_by_their_variant_heading() {
        let files = parse_firmware_files(BOARD_ESP32_GENERIC);
        assert!(
            files
                .iter()
                .any(|f| f.variant == "ESP32 D2WD" && f.kind == FirmwareKind::Bin)
        );
        assert!(
            files
                .iter()
                .any(|f| f.variant.contains("SPIRAM") && f.kind == FirmwareKind::Bin)
        );
    }

    #[test]
    fn app_bin_and_elf_are_parsed_but_distinguishable_from_bin() {
        let files = parse_firmware_files(BOARD_ESP32_GENERIC);
        assert!(files.iter().any(|f| f.kind == FirmwareKind::AppBin));
        assert!(files.iter().any(|f| f.kind == FirmwareKind::Elf));
        // Only `.bin` is meant to be offered as a flashable candidate.
        let bin_count = files.iter().filter(|f| f.kind == FirmwareKind::Bin).count();
        let app_bin_count = files
            .iter()
            .filter(|f| f.kind == FirmwareKind::AppBin)
            .count();
        assert!(bin_count > 0 && app_bin_count > 0 && bin_count >= app_bin_count);
    }

    #[test]
    fn a_page_with_no_firmware_section_yields_an_empty_list() {
        assert!(parse_firmware_files("<html><body>nothing here</body></html>").is_empty());
    }
}
