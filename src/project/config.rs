//! Project-local scaffold file: `chiptui.toml`.
//!
//! `SPEC.md` §7: once the user answers the empty-project prompt, the choice
//! is persisted here so the directory is recognized automatically on every
//! later run. The format is a single key, so this is a hand-rolled tolerant
//! parser rather than a `toml` dependency --- the same bias the rest of the
//! codebase has for small, focused parsers (`esptool::parse`,
//! `micropython::parse`) over pulling in a crate for one field.

use std::io;
use std::path::Path;

use crate::backend::BackendKind;
use crate::backend::zephyr::variants::{Variant, VariantOrigin};
use crate::ota::{OtaConfig, OtaMethod, Transport};

/// The scaffold file's name, at the project root.
pub const FILE_NAME: &str = "chiptui.toml";

/// Reads the `[[variant]]` blocks out of `text`: the project's own
/// declaration of the build configurations it keeps in parallel.
///
/// ```toml
/// [[variant]]
/// name = "sim"
/// board = "native_sim/native/64"
/// build_dir = "build-sim"
/// ```
///
/// Only `name` makes a block; `board`, `shield` and `build_dir` are each
/// optional, and a missing `build_dir` falls back to the conventional
/// `build`. A block without a name is skipped rather than fatal --- the
/// same tolerance every other hand-rolled parser here has, since a file a
/// newer ChipTUI wrote must not break an older one.
///
/// ChipTUI never writes this file (`SPEC.md` §7): it is here because the
/// user put it here, typically to commit it so the team shares the
/// variants. A project that declares none has them discovered instead
/// ([`crate::backend::zephyr::variants::discover`]).
pub fn parse_variants(text: &str) -> Vec<Variant> {
    let mut variants: Vec<Variant> = Vec::new();
    let mut pending: Option<PendingVariant> = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            variants.extend(pending.take().and_then(PendingVariant::finish));
            if line == "[[variant]]" {
                pending = Some(PendingVariant::default());
            }
            continue;
        }
        let Some(block) = pending.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" => block.name = Some(value),
            "board" => block.board = Some(value),
            "shield" => block.shield = Some(value),
            "build_dir" => block.build_dir = Some(value),
            _ => {}
        }
    }
    variants.extend(pending.and_then(PendingVariant::finish));
    variants
}

/// A `[[variant]]` block being read; becomes a [`Variant`] only if it
/// carries the one key that makes it meaningful.
#[derive(Default)]
struct PendingVariant {
    name: Option<String>,
    board: Option<String>,
    shield: Option<String>,
    build_dir: Option<String>,
}

impl PendingVariant {
    fn finish(self) -> Option<Variant> {
        let name = self.name.filter(|name| !name.is_empty())?;
        Some(Variant {
            name,
            board: self.board.filter(|board| !board.is_empty()),
            shield: self.shield.filter(|shield| !shield.is_empty()),
            build_dir: self
                .build_dir
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| crate::build::DEFAULT_BUILD_DIR.to_string()),
            origin: VariantOrigin::Declared,
        })
    }
}

/// Strips one layer of quoting, the way [`crate::settings`] does. Kept
/// local rather than shared: the two files are parsed by two modules on
/// purpose, and one small function is a cheaper coupling than none.
fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value)
        .to_string()
}

/// Reads `project_type = "<id>"` out of `text`, ignoring comments, blank
/// lines and surrounding whitespace. Returns `None` for anything else ---
/// a missing or unrecognised value falls back to normal detection rather
/// than failing outright.
pub fn parse(text: &str) -> Option<BackendKind> {
    text.lines().find_map(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        let value = line.strip_prefix("project_type")?.trim();
        let value = value.strip_prefix('=')?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))?;
        BackendKind::from_id(value)
    })
}

/// The section a project's over-the-air answers live in.
pub const OTA_SECTION: &str = "ota";

/// Reads `[ota]` out of `text`.
///
/// Section-aware, unlike [`parse`]. `project_type` is a top-level key with
/// a name nothing else uses, so a bare scan is safe for it; `address` is
/// not --- a `[[variant]]` or a future section could carry one, and reading
/// the wrong file's answer is worse than reading none.
///
/// `None` means the project has no `[ota]` section at all. A section that
/// exists but names an unknown mechanism or transport also reads as `None`:
/// silently falling back to the default would tell the user their answer
/// was accepted and then run a different one.
pub fn parse_ota(text: &str) -> Option<OtaConfig> {
    let keys = section_keys(text, OTA_SECTION)?;
    let lookup = |name: &str| {
        keys.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    let method = match lookup("method") {
        Some(id) => OtaMethod::from_id(id)?,
        None => OtaMethod::default(),
    };
    let transport = match lookup("transport") {
        Some(id) => Transport::from_id(id)?,
        None => Transport::default(),
    };
    // Unknown values read as `None` for the whole section, the same rule
    // the mechanism and the transport follow: accepting a misspelled
    // `auto_confirm = fasle` as its default would tell the user their
    // answer was taken and then do the opposite of it --- and here the
    // opposite is a permanent image.
    let auto_confirm = match lookup("auto_confirm") {
        Some("true") => true,
        Some("false") => false,
        Some(_) => return None,
        None => OtaConfig::default().auto_confirm,
    };
    Some(OtaConfig {
        method,
        transport,
        address: lookup("address")
            .filter(|address| !address.is_empty())
            .map(str::to_string),
        auto_confirm,
    })
}

/// The `key = value` pairs of one section, in source order. `None` when the
/// section is absent --- which is a different answer from "present and
/// empty", and the caller distinguishes them.
fn section_keys(text: &str, section: &str) -> Option<Vec<(String, String)>> {
    let mut found = false;
    let mut keys = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            // `[ota]` and not `[[ota]]`: an array of tables is a different
            // shape and is not what this reads.
            inside = line == format!("[{section}]");
            found |= inside;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            keys.push((key.trim().to_string(), unquote(value.trim())));
        }
    }
    found.then_some(keys)
}

/// Sets one `[section] key` in the project's `chiptui.toml`, creating the
/// file if it is absent and leaving every other byte of it alone.
///
/// The project-side twin of `settings::save_*`, sharing their algorithm
/// rather than repeating it: `upsert_key` *is* the preservation guarantee,
/// and `write_config` is the atomicity one (a temporary file beside the
/// target, then a rename), so a half-written `chiptui.toml` cannot replace
/// a whole one.
///
/// This writes plain `[section] key = value`. An array of tables ---
/// `[[variant]]` --- is a different shape needing a different writer;
/// nothing writes those yet, so nothing here does either.
pub fn set_key(path: &Path, section: &str, key: &str, value: &str) -> io::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let updated = crate::settings::upsert_key(&text, section, key, value);
    crate::settings::write_config(path, &updated)
}

/// Writes a project's `[ota]` answers.
///
/// One [`set_key`] per answered field, and nothing for an unanswered one: a
/// key ChipTUI has no answer for must not appear as an empty string, which
/// [`parse_ota`] would then have to treat as an answer.
///
/// The consequence, deliberate: an address already in the file survives a
/// save that carries none. Writing nothing is how "I have no answer" is
/// expressed here, so it cannot also mean "remove theirs" --- and of the
/// two readings, keeping what the user typed is the one that loses no
/// information. Clearing an address is a different act and would need a
/// different function.
///
/// Note what is *not* written: whether the project is already instrumented.
/// That is answered by reading the project --- `sysbuild.conf`, `VERSION`,
/// the Kconfig block --- not by a flag recorded here. A flag would be a
/// second truth, and the first one to drift.
pub fn save_ota(path: &Path, config: &OtaConfig) -> io::Result<()> {
    set_key(path, OTA_SECTION, "method", config.method.id())?;
    set_key(path, OTA_SECTION, "transport", config.transport.id())?;
    // Written even at its default, unlike the address: it is a bool, so it
    // always has an answer, and a key the user can see is the whole point
    // of a setting that decides whether an update can still be reverted.
    set_key(
        path,
        OTA_SECTION,
        "auto_confirm",
        if config.auto_confirm { "true" } else { "false" },
    )?;
    if let Some(address) = &config.address {
        set_key(path, OTA_SECTION, "address", address)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_blocks_are_read_with_their_optional_halves() {
        let variants = parse_variants(
            "project_type = \"zephyr\"\n\n\
             [[variant]]\n\
             name = \"hardware\"\n\
             board = \"xiao_esp32c3\"\n\
             shield = 'seeed_xiao_round_display'  # committed answer\n\
             \n\
             [[variant]]\n\
             name = \"sim\"\n\
             board = \"native_sim/native/64\"\n\
             build_dir = \"build_sim\"\n",
        );
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].name, "hardware");
        assert_eq!(variants[0].board.as_deref(), Some("xiao_esp32c3"));
        assert_eq!(
            variants[0].shield.as_deref(),
            Some("seeed_xiao_round_display")
        );
        // No `build_dir`: the conventional default, so the common case
        // needs no line.
        assert_eq!(variants[0].build_dir, "build");
        assert_eq!(variants[1].build_dir, "build_sim");
        assert!(variants[1].is_simulator());
        assert_eq!(variants[1].origin, VariantOrigin::Declared);
    }

    /// A block without a name is not a variant, and an unrelated section
    /// closes the block rather than leaking into it --- the same tolerance
    /// the registry parser has.
    #[test]
    fn a_nameless_block_and_a_foreign_section_are_ignored() {
        let variants = parse_variants(
            "[[variant]]\nboard = \"a\"\n\n[zephyr]\nworkspace = \"/w\"\n\n             [[variant]]\nname = \"only\"\n",
        );
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].name, "only");
        assert_eq!(variants[0].board, None);
    }

    #[test]
    fn a_file_without_variants_declares_none() {
        assert!(parse_variants("project_type = \"zephyr\"\n").is_empty());
        assert!(parse_variants("").is_empty());
    }

    #[test]
    fn parses_a_double_quoted_value() {
        assert_eq!(
            parse("project_type = \"micropython\"\n"),
            Some(BackendKind::MicroPython)
        );
    }

    #[test]
    fn parses_a_single_quoted_value_and_tolerates_no_spaces() {
        assert_eq!(parse("project_type='zephyr'"), Some(BackendKind::Zephyr));
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let text = "\n# a comment\nproject_type = \"zephyr\" # trailing note\n";
        assert_eq!(parse(text), Some(BackendKind::Zephyr));
    }

    #[test]
    fn unknown_or_missing_value_yields_none() {
        assert_eq!(parse("project_type = \"esp-idf\"\n"), None);
        assert_eq!(parse("[project]\nname = \"demo\"\n"), None);
        assert_eq!(parse(""), None);
    }

    /// A project file with everything a hand-written one carries: a
    /// comment, the project type, variant blocks, an unknown section and an
    /// unknown key. Every test below asserts this survives.
    const HAND_WRITTEN: &str = "\
# our board, committed so the team shares the variants
project_type = \"zephyr\"

[[variant]]
name = \"hardware\"
board = \"xiao_esp32c3\"

[[variant]]
name = \"sim\"
board = \"native_sim/native/64\"
build_dir = \"build_sim\"

[future]
something = \"a newer chiptui wrote this\"
";

    fn temp_file(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-ota-config-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE_NAME)
    }

    #[test]
    fn an_absent_section_is_none_and_a_written_one_round_trips() {
        assert_eq!(parse_ota(HAND_WRITTEN), None);

        let path = temp_file("roundtrip");
        std::fs::write(&path, HAND_WRITTEN).unwrap();
        let config = OtaConfig {
            method: OtaMethod::Mcumgr,
            transport: Transport::Serial,
            address: Some("/dev/ttyACM0".to_string()),
            auto_confirm: false,
        };
        save_ota(&path, &config).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        assert_eq!(parse_ota(&text), Some(config));
    }

    #[test]
    fn writing_ota_leaves_every_other_byte_alone() {
        let path = temp_file("preserve");
        std::fs::write(&path, HAND_WRITTEN).unwrap();

        save_ota(
            &path,
            &OtaConfig {
                address: Some("192.168.1.42".to_string()),
                ..OtaConfig::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        // The whole original file is still in there, in order: the comment,
        // the project type, both variants and the section a newer ChipTUI
        // wrote. This is the promise that makes writing into someone's
        // repository defensible at all.
        for line in HAND_WRITTEN.lines().filter(|line| !line.trim().is_empty()) {
            assert!(text.contains(line), "lost {line:?} from:\n{text}");
        }
        assert_eq!(parse_variants(&text).len(), 2, "{text}");
        assert_eq!(parse(&text), Some(BackendKind::Zephyr), "{text}");
    }

    #[test]
    fn a_second_write_replaces_rather_than_appends() {
        let path = temp_file("idempotent");

        save_ota(
            &path,
            &OtaConfig {
                address: Some("192.168.1.42".into()),
                ..Default::default()
            },
        )
        .unwrap();
        save_ota(
            &path,
            &OtaConfig {
                address: Some("192.168.1.99".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        assert_eq!(text.matches("[ota]").count(), 1, "{text}");
        assert_eq!(text.matches("address").count(), 1, "{text}");
        assert_eq!(
            parse_ota(&text).unwrap().address.as_deref(),
            Some("192.168.1.99")
        );
    }

    #[test]
    fn a_hand_written_section_with_only_an_address_is_a_complete_answer() {
        // The common case: the mechanism and transport are what a Zephyr
        // project almost always wants, and the address is the one fact
        // ChipTUI cannot know.
        let config = parse_ota("[ota]\naddress = \"192.168.1.42\"\n").unwrap();

        assert_eq!(config.method, OtaMethod::Mcumgr);
        assert_eq!(config.transport, Transport::Udp);
        assert_eq!(config.address.as_deref(), Some("192.168.1.42"));
    }

    #[test]
    fn auto_confirm_defaults_to_on_and_is_read_in_both_spellings() {
        // Absent: a verified swap confirms itself, which is the default the
        // whole update flow is written around.
        assert!(
            parse_ota("[ota]\naddress = \"192.168.1.42\"\n")
                .unwrap()
                .auto_confirm
        );
        // Written by `save_ota` (quoted, like every key this writer emits)
        // and hand-written as a TOML bool: both are the same answer.
        assert!(
            !parse_ota("[ota]\nauto_confirm = \"false\"\n")
                .unwrap()
                .auto_confirm
        );
        assert!(
            !parse_ota("[ota]\nauto_confirm = false\n")
                .unwrap()
                .auto_confirm
        );
        assert!(
            parse_ota("[ota]\nauto_confirm = true\n")
                .unwrap()
                .auto_confirm
        );
        // And a value neither: refused whole, the rule an unknown mechanism
        // follows. Reading `fasle` as the default would report the answer
        // as taken and then make the image permanent anyway.
        assert_eq!(parse_ota("[ota]\nauto_confirm = \"fasle\"\n"), None);
    }

    #[test]
    fn an_address_in_another_section_is_not_read_as_the_ota_one() {
        // The trap `settings::upsert_key`'s own comment names, asserted for
        // this reader: a key of the same name elsewhere is a different key.
        let text = "[future]\naddress = \"nothing to do with ota\"\n\n[ota]\ntransport = \"ble\"\n";
        let config = parse_ota(text).unwrap();

        assert_eq!(config.transport, Transport::Ble);
        assert_eq!(config.address, None);
    }

    #[test]
    fn an_unknown_mechanism_is_refused_rather_than_defaulted() {
        // Reading a file that names something this build cannot do as
        // though it named the default would run the wrong mechanism while
        // telling the user their answer was taken.
        assert_eq!(parse_ota("[ota]\nmethod = \"http-pull\"\n"), None);
        assert_eq!(parse_ota("[ota]\ntransport = \"usb\"\n"), None);
    }

    #[test]
    fn the_file_is_created_when_the_project_has_none() {
        let path = temp_file("create");

        save_ota(&path, &OtaConfig::default()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        assert_eq!(parse_ota(&text), Some(OtaConfig::default()));
    }

    #[test]
    fn every_backend_id_is_readable_as_a_project_type() {
        // The property the deleted `render`/`write` pair used to carry: a
        // file naming any backend reads back as that backend. Asserted
        // against the spelling a user would write, since that is now the
        // only way this file is produced.
        for kind in BackendKind::ALL {
            let text = format!("project_type = \"{}\"\n", kind.id());
            assert_eq!(parse(&text), Some(*kind));
        }
    }
}
