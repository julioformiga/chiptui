//! User-level and `[zephyr]` configuration parsing.
//!
//! `SPEC.md` §13: user configuration lives outside the project, project
//! configuration inside `chiptui.toml`. Both need the same `[zephyr]` section
//! (workspace / sdk / west paths), so the parsing lives here once and both
//! readers call it. Like `project::config`, this is a hand-rolled tolerant
//! parser rather than a `toml` dependency --- the section has three string
//! keys, and the same bias against pulling in a crate for one shape applies.
//!
//! Nothing here touches the filesystem beyond the one read in
//! [`load_user`]: callers decide what a missing file means (for the user
//! config, "not configured yet" --- not an error).

use std::path::{Path, PathBuf};

/// The `[zephyr]` section's shape, shared by both config levels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZephyrSettings {
    /// The west workspace root: the directory holding `.west/`, the Zephyr
    /// checkout and (by convention) `.venv/`.
    pub workspace: Option<String>,
    /// Optional toolchain location, exported as `ZEPHYR_SDK_INSTALL_DIR`.
    pub sdk: Option<String>,
    /// Optional explicit west executable; without one, `<workspace>/.venv/
    /// bin/west` is used when it exists, then `west` from `PATH`.
    pub west: Option<String>,
}

impl ZephyrSettings {
    /// Whether the section carries anything usable.
    pub fn is_empty(&self) -> bool {
        self.workspace.is_none() && self.sdk.is_none() && self.west.is_none()
    }

    /// Extracts the `[zephyr]` section from `text`, ignoring comments, other
    /// sections and blank lines. Values may be single- or double-quoted;
    /// `~` is kept verbatim (expansion needs a home directory, resolved by
    /// the caller --- see [`expand_home`]).
    pub fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        let mut in_section = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_section = name.trim() == "zephyr";
                continue;
            }
            if !in_section {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = unquote(value.trim());
            let slot = match key {
                "workspace" => &mut settings.workspace,
                "sdk" => &mut settings.sdk,
                "west" => &mut settings.west,
                _ => continue,
            };
            if !value.is_empty() {
                *slot = Some(value);
            }
        }
        settings
    }
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value)
        .to_string()
}

/// Resolves a leading `~` against `home`. Absolute paths pass through;
/// relative paths are left alone for the caller to reject (a workspace that
/// moves with the cwd would be a different workspace per project).
pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// The user config's location: `$XDG_CONFIG_HOME/chiptui/config.toml`, or
/// `~/.config/chiptui/config.toml` when XDG names no directory. This is the
/// path the unresolved-workspace pane tells the user about, so it must be
/// the one the app actually reads.
pub fn user_config_path(home: &Path) -> PathBuf {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    config_home.join("chiptui/config.toml")
}

/// Reads the user config, if it exists. A missing file is `None` --- "not
/// configured yet", never an error (`SPEC.md` §13's project levels stay
/// optional).
pub fn load_user(home: &Path) -> Option<ZephyrSettings> {
    let text = std::fs::read_to_string(user_config_path(home)).ok()?;
    let settings = ZephyrSettings::parse(&text);
    (!settings.is_empty()).then_some(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_zephyr_section_with_comments_and_quotes() {
        let text = "# top comment\n\
                    project_type = \"zephyr\"\n\
                    [ui]\nmouse = false\n\
                    [zephyr]\n\
                    # where the workspace lives\n\
                    workspace = \"~/zephyrproject\"\n\
                    sdk = '~/zephyr-sdk-0.17.1'\n\
                    west = \"/opt/west\"\n";
        let settings = ZephyrSettings::parse(text);
        assert_eq!(settings.workspace.as_deref(), Some("~/zephyrproject"));
        assert_eq!(settings.sdk.as_deref(), Some("~/zephyr-sdk-0.17.1"));
        assert_eq!(settings.west.as_deref(), Some("/opt/west"));
    }

    #[test]
    fn keys_outside_the_section_are_ignored() {
        let settings = ZephyrSettings::parse("workspace = \"/elsewhere\"\n[other]\nwest = \"x\"\n");
        assert!(settings.is_empty(), "only [zephyr] keys count");
    }

    #[test]
    fn a_section_ends_where_the_next_begins() {
        let text = "[zephyr]\nworkspace = \"~/ws\"\n[zephyr-extra]\nwest = \"nope\"\n";
        let settings = ZephyrSettings::parse(text);
        assert_eq!(settings.workspace.as_deref(), Some("~/ws"));
        assert_eq!(settings.west, None, "[zephyr-extra] is another section");
    }

    #[test]
    fn empty_values_are_ignored_rather_than_clearing() {
        let settings = ZephyrSettings::parse("[zephyr]\nworkspace = \"\"\nsdk =\n");
        assert!(settings.is_empty());
    }

    #[test]
    fn home_expansion_handles_all_three_spellings() {
        let home = Path::new("/home/dev");
        assert_eq!(expand_home("~", home), PathBuf::from("/home/dev"));
        assert_eq!(
            expand_home("~/zephyrproject", home),
            PathBuf::from("/home/dev/zephyrproject")
        );
        assert_eq!(
            expand_home("/opt/ws", home),
            PathBuf::from("/opt/ws"),
            "absolute paths pass through"
        );
    }
}
