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
    /// Where the user's Zephyr *applications* live --- any directory, the
    /// projects the build panel's picker lists (`SPEC.md` §13).
    pub projects: Option<String>,
    /// Optional toolchain location, exported as `ZEPHYR_SDK_INSTALL_DIR`.
    pub sdk: Option<String>,
    /// Optional explicit west executable; without one, `<workspace>/.venv/
    /// bin/west` is used when it exists, then `west` from `PATH`.
    pub west: Option<String>,
}

impl ZephyrSettings {
    /// Whether the section carries anything usable.
    pub fn is_empty(&self) -> bool {
        self.workspace.is_none()
            && self.projects.is_none()
            && self.sdk.is_none()
            && self.west.is_none()
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
                "projects" => &mut settings.projects,
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

/// Persists `workspace = dir` into the `[zephyr]` section of the config at
/// `path`, creating the file (and its parent directories) when needed.
///
/// This is how the directory picker's answer outlives the session: the
/// choice is written to the user config (or the project's `chiptui.toml`
/// when the project pins its own location), so the next start resolves from
/// the file instead of asking again. Everything else in the file survives:
/// only the one line is replaced or inserted, and other keys (`sdk`,
/// `west`) and sections are left byte-for-byte as they were.
pub fn save_workspace(config: &Path, dir: &Path) -> std::io::Result<()> {
    save_zephyr_key(config, "workspace", &dir.display().to_string())
}

/// Persists `projects = dir` the same way [`save_workspace`] persists the
/// installation: one line replaced or inserted, everything else untouched.
pub fn save_projects(config: &Path, dir: &Path) -> std::io::Result<()> {
    save_zephyr_key(config, "projects", &dir.display().to_string())
}

fn save_zephyr_key(config: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let text = std::fs::read_to_string(config).unwrap_or_default();
    let updated = upsert_key(&text, key, value);
    if let Some(parent) = config.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(config, updated)
}

/// The pure half of [`save_workspace`]/[`save_projects`], so the merge is
/// testable without touching the filesystem.
fn upsert_key(text: &str, key: &str, value: &str) -> String {
    let section_header = |line: &str| {
        line.split('#')
            .next()
            .unwrap_or("")
            .trim()
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
            .is_some_and(|name| name.trim() == "zephyr")
    };

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut in_section = false;
    let mut replaced = false;
    for line in &mut lines {
        if section_header(line) {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        let stripped = line.split('#').next().unwrap_or("").trim();
        if let Some((found, _)) = stripped.split_once('=')
            && found.trim() == key
        {
            *line = format!("{key} = \"{value}\"");
            replaced = true;
            break;
        }
    }
    if !replaced {
        if let Some(index) = lines.iter().position(|line| section_header(line)) {
            lines.insert(index + 1, format!("{key} = \"{value}\""));
        } else {
            lines.push(String::new());
            lines.push("[zephyr]".to_string());
            lines.push(format!("{key} = \"{value}\""));
        }
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
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

    #[test]
    fn save_creates_the_file_and_round_trips() {
        let dir =
            std::env::temp_dir().join(format!("chiptui-save-{}-{}", std::process::id(), line!()));
        let config = dir.join("chiptui/config.toml");
        save_workspace(&config, Path::new("/opt/myzephyr")).unwrap();

        let text = std::fs::read_to_string(&config).unwrap();
        assert_eq!(
            ZephyrSettings::parse(&text).workspace.as_deref(),
            Some("/opt/myzephyr")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_replaces_only_the_workspace_line() {
        let existing = "# my setup\n[zephyr]\nworkspace = \"/old\"\nsdk = \"~/sdk-0.17\"\n\n[ui]\nmouse = false\n";
        let updated = upsert_key(existing, "workspace", "/opt/myzephyr");
        assert!(updated.contains("workspace = \"/opt/myzephyr\""));
        assert!(!updated.contains("/old"));
        assert!(
            updated.contains("sdk = \"~/sdk-0.17\""),
            "sibling keys survive:\n{updated}"
        );
        assert!(
            updated.contains("[ui]\nmouse = false"),
            "other sections survive:\n{updated}"
        );
        assert!(updated.contains("# my setup"), "comments survive");
    }

    #[test]
    fn save_inserts_into_an_existing_section_without_workspace() {
        let updated = upsert_key("[zephyr]\nsdk = \"~/sdk\"\n", "workspace", "/opt/myzephyr");
        assert_eq!(
            updated,
            "[zephyr]\nworkspace = \"/opt/myzephyr\"\nsdk = \"~/sdk\"\n"
        );
    }

    #[test]
    fn save_appends_the_section_when_absent() {
        let updated = upsert_key("[ui]\nmouse = false\n", "workspace", "/opt/myzephyr");
        assert!(updated.contains("[ui]\nmouse = false"));
        assert!(updated.ends_with("[zephyr]\nworkspace = \"/opt/myzephyr\"\n"));
    }

    #[test]
    fn save_projects_touches_only_the_projects_line() {
        let existing = "[zephyr]\nworkspace = \"/ws\"\nprojects = \"/old\"\n";
        let updated = upsert_key(existing, "projects", "/opt/apps");
        assert_eq!(
            updated,
            "[zephyr]\nworkspace = \"/ws\"\nprojects = \"/opt/apps\"\n"
        );
    }
}
