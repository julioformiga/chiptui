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

/// The file's contents for `kind`.
pub fn render(kind: BackendKind) -> String {
    format!(
        "# Written by ChipTUI --- records the project type chosen for this directory.\nproject_type = \"{}\"\n",
        kind.id()
    )
}

/// Writes the scaffold file to `dir`.
pub fn write(dir: &Path, kind: BackendKind) -> io::Result<()> {
    std::fs::write(dir.join(FILE_NAME), render(kind))
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

    #[test]
    fn render_round_trips_through_parse() {
        for kind in BackendKind::ALL {
            assert_eq!(parse(&render(*kind)), Some(*kind));
        }
    }

    #[test]
    fn write_creates_a_readable_file() {
        let dir =
            std::env::temp_dir().join(format!("chiptui-config-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();

        write(&dir, BackendKind::MicroPython).unwrap();
        let text = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(parse(&text), Some(BackendKind::MicroPython));
    }
}
