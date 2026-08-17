//! Resolves the terminal's default editor for the file viewer's `e` key.
//!
//! `SPEC.md` §2's secondary goal is to *support* external editors, not embed
//! one (§3 excludes that outright), so this only decides what to run.
//! `AGENTS.md` §5: commands are structured, never shell strings --- the value
//! of `$VISUAL`/`$EDITOR` is split on whitespace, not handed to a shell, so a
//! value like `code -w` works but one relying on shell quoting does not.

use std::path::{Path, PathBuf};

/// A program plus arguments, ready to append the target path to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// POSIX's own fallback when neither environment variable is set.
const FALLBACK: &str = "vi";

/// Reads `$VISUAL` then `$EDITOR` from the environment and resolves them.
pub fn resolve() -> EditorCommand {
    let raw = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok());
    parse(raw.as_deref())
}

/// Pure parsing, kept separate from [`resolve`] so tests do not have to
/// mutate process-global environment variables (which would race other
/// tests running in parallel).
fn parse(value: Option<&str>) -> EditorCommand {
    let value = value.filter(|v| !v.trim().is_empty()).unwrap_or(FALLBACK);
    let mut parts = value.split_whitespace();
    let program = parts.next().unwrap_or(FALLBACK).to_string();
    let args = parts.map(str::to_string).collect();
    EditorCommand { program, args }
}

/// The path `$EDITOR` is handed for `path` when it runs *from* `cwd` (the
/// project folder): relative to it when the file lies inside --- the
/// `cd project && $EDITOR src/main.c` spelling, so an editor whose file
/// explorer follows its working directory opens straight on the project's
/// files --- and absolute otherwise, since a path outside the project (a
/// device file's scratch copy, say) has no useful relative spelling.
pub fn target_from(path: &Path, cwd: &Path) -> PathBuf {
    match path.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_path_buf(),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_vi_when_unset() {
        assert_eq!(parse(None).program, "vi");
        assert!(parse(None).args.is_empty());
    }

    #[test]
    fn falls_back_to_vi_when_set_but_blank() {
        assert_eq!(parse(Some("   ")).program, "vi");
    }

    #[test]
    fn a_plain_editor_name_has_no_arguments() {
        let command = parse(Some("nvim"));
        assert_eq!(command.program, "nvim");
        assert!(command.args.is_empty());
    }

    #[test]
    fn extra_words_become_arguments() {
        let command = parse(Some("code -w"));
        assert_eq!(command.program, "code");
        assert_eq!(command.args, ["-w"]);
    }

    #[test]
    fn extra_whitespace_is_collapsed() {
        let command = parse(Some("  vim   -u  NONE  "));
        assert_eq!(command.program, "vim");
        assert_eq!(command.args, ["-u", "NONE"]);
    }

    #[test]
    fn a_file_inside_the_cwd_is_addressed_relatively() {
        let target = target_from(Path::new("/p/src/main.c"), Path::new("/p"));
        assert_eq!(target, Path::new("src/main.c"));
    }

    #[test]
    fn a_file_outside_the_cwd_keeps_its_absolute_spelling() {
        let target = target_from(Path::new("/tmp/chiptui-edit-0/main.py"), Path::new("/p"));
        assert_eq!(target, Path::new("/tmp/chiptui-edit-0/main.py"));
    }

    #[test]
    fn the_cwd_itself_is_not_an_empty_relative_path() {
        let target = target_from(Path::new("/p"), Path::new("/p"));
        assert_eq!(target, Path::new("/p"));
    }
}
