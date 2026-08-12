//! Resolves the terminal's default editor for the file viewer's `e` key.
//!
//! `SPEC.md` §2's secondary goal is to *support* external editors, not embed
//! one (§3 excludes that outright), so this only decides what to run.
//! `AGENTS.md` §5: commands are structured, never shell strings --- the value
//! of `$VISUAL`/`$EDITOR` is split on whitespace, not handed to a shell, so a
//! value like `code -w` works but one relying on shell quoting does not.

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
}
