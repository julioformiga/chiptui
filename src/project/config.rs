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

/// The scaffold file's name, at the project root.
pub const FILE_NAME: &str = "chiptui.toml";

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
