//! User-level and `[zephyr]` configuration parsing, plus the project
//! registry the home screen lists.
//!
//! `SPEC.md` §13: user configuration lives outside the project, project
//! configuration inside `chiptui.toml`. Both need the same `[zephyr]` section
//! (workspace / sdk / west paths), so the parsing lives here once and both
//! readers call it. Like `project::config`, this is a hand-rolled tolerant
//! parser rather than a `toml` dependency --- the section has three string
//! keys, and the same bias against pulling in a crate for one shape applies.
//!
//! The same file carries the `[[project]]` blocks ([`ProjectRegistry`]):
//! which directories are ChipTUI projects, which backend each one is, and
//! the per-project target answers (Zephyr board and shield) that must
//! reload when the project opens. That is what replaced writing a marker
//! file into every project directory
//! --- ChipTUI still *reads* a project's `chiptui.toml` when one exists, but
//! it no longer creates one (`SPEC.md` §7).
//!
//! Nothing here touches the filesystem beyond the one read in
//! [`load_user`]: callers decide what a missing file means (for the user
//! config, "not configured yet" --- not an error).

use std::path::{Path, PathBuf};

use crate::backend::BackendKind;

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
    /// bin/west` is used when it exists, then `west` from `PATH`. A
    /// relative path is resolved against the workspace, and a bare program
    /// name is a deliberate `PATH` lookup.
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
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        // Only the two escapes this module ever *writes* are undone, so a
        // hand-written Windows path (`C:\dev`, unescaped) survives verbatim
        // instead of losing its separators to a general unescaper.
        return inner.replace("\\\"", "\"").replace("\\\\", "\\");
    }
    value
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .unwrap_or(value)
        .to_string()
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A directory ChipTUI knows is a project, as recorded in the user config.
///
/// This is the persisted answer to "which backend is this directory?" that
/// used to live in a per-project `chiptui.toml`, plus what the home screen
/// needs to list it, plus the Zephyr target answers (board, shield) that
/// must reload every time the project opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    /// Absolute path of the project root.
    pub path: PathBuf,
    pub backend: BackendKind,
    /// Shown by the home screen; the directory's own name unless the config
    /// was hand-edited to something friendlier.
    pub name: String,
    /// When the project was last opened, `YYYY-MM-DDThh:mm:ssZ` (UTC, fixed
    /// width). Kept as the written string rather than a parsed timestamp:
    /// the only thing the app does with it is order the home screen, and at
    /// fixed width and one timezone that is a string compare. `None` for an
    /// entry that was hand-written or never opened since.
    pub last_opened: Option<String>,
    /// The board picker's answer, persisted: re-applied when the project
    /// opens, outranking the build directory's cache until the user picks
    /// again. Still never written into the project directory --- the
    /// registry is the persisted half of a session answer (`SPEC.md` §10).
    pub board: Option<String>,
    /// The shield picker's answer, same lifetime as [`Self::board`].
    pub shield: Option<String>,
}

impl ProjectEntry {
    /// An entry for `path`, named after the directory.
    pub fn new(path: impl Into<PathBuf>, backend: BackendKind) -> Self {
        let path = path.into();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            path,
            backend,
            name,
            last_opened: None,
            board: None,
            shield: None,
        }
    }

    /// The same entry stamped with the current time --- what recording an
    /// *opened* project writes.
    pub fn opened_now(self) -> Self {
        Self {
            last_opened: Some(now_stamp()),
            ..self
        }
    }
}

/// `YYYY-MM-DDThh:mm:ssZ` for the current instant, formatted by hand: the
/// `time` dependency is built without its `formatting` feature (`logs.rs`
/// renders its stamps the same way), and this format's whole job is to sort
/// lexicographically.
fn now_stamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Every `[[project]]` block in the user config.
///
/// Loaded once at startup and carried as a value: it answers both "does
/// opening this directory need the home screen at all?"
/// ([`Self::backend_for`], consulted by detection) and "what does the home
/// screen list?" ([`Self::listed`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRegistry {
    entries: Vec<ProjectEntry>,
}

impl ProjectRegistry {
    /// Reads the registry out of `text`, ignoring everything else in the
    /// file. A block without a `path`, or with a backend this build does not
    /// know, is skipped rather than failing the load --- a config written by
    /// a newer ChipTUI must not break an older one.
    pub fn parse(text: &str) -> Self {
        let mut entries = Vec::new();
        let mut current: Option<PendingEntry> = None;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                if let Some(pending) = current.take() {
                    entries.extend(pending.finish());
                }
                if line == "[[project]]" {
                    current = Some(PendingEntry::default());
                }
                continue;
            }
            let Some(pending) = current.as_mut() else {
                continue;
            };
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unquote(value.trim());
            match key.trim() {
                "path" => pending.path = Some(value),
                "backend" => pending.backend = BackendKind::from_id(&value),
                "name" => pending.name = Some(value),
                "last_opened" => pending.last_opened = Some(value),
                "board" => pending.board = Some(value),
                "shield" => pending.shield = Some(value),
                _ => {}
            }
        }
        if let Some(pending) = current {
            entries.extend(pending.finish());
        }
        Self { entries }
    }

    /// The registry in the user config at `config_dir`, with `~` in every
    /// path resolved against `home`. A missing or unreadable file is an
    /// empty registry --- "nothing recorded yet", never an error.
    pub fn load(config_dir: &Path, home: &Path) -> Self {
        let text = std::fs::read_to_string(user_config_path(config_dir)).unwrap_or_default();
        let mut registry = Self::parse(&text);
        for entry in &mut registry.entries {
            entry.path = expand_home(&entry.path.to_string_lossy(), home);
        }
        registry
    }

    pub fn entries(&self) -> &[ProjectEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry recorded for `dir`, if any. Exact path match: the upward
    /// search for a project root belongs to detection, which calls this
    /// once per ancestor.
    pub fn entry_for(&self, dir: &Path) -> Option<&ProjectEntry> {
        self.entries.iter().find(|entry| entry.path == dir)
    }

    /// The backend recorded for `dir`, if any.
    pub fn backend_for(&self, dir: &Path) -> Option<BackendKind> {
        self.entry_for(dir).map(|entry| entry.backend)
    }

    /// What the home screen shows: entries whose directory still exists,
    /// most recently opened first, then alphabetically by name (which is
    /// also the whole order for a registry no session has stamped yet).
    ///
    /// Directories that vanished are omitted here and dropped from the file
    /// by the next [`record_project`] --- a moved or deleted project should
    /// not be a row that fails when picked.
    pub fn listed(&self) -> Vec<&ProjectEntry> {
        let mut listed: Vec<&ProjectEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.path.is_dir())
            .collect();
        listed.sort_by(|a, b| {
            b.last_opened
                .cmp(&a.last_opened)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.path.cmp(&b.path))
        });
        listed
    }
}

/// A `[[project]]` block being read; becomes a [`ProjectEntry`] only if it
/// carries the two keys that make it meaningful.
#[derive(Default)]
struct PendingEntry {
    path: Option<String>,
    backend: Option<BackendKind>,
    name: Option<String>,
    last_opened: Option<String>,
    board: Option<String>,
    shield: Option<String>,
}

impl PendingEntry {
    fn finish(self) -> Option<ProjectEntry> {
        let path = self.path.filter(|path| !path.is_empty())?;
        let backend = self.backend?;
        let mut entry = ProjectEntry::new(PathBuf::from(path), backend);
        if let Some(name) = self.name.filter(|name| !name.is_empty()) {
            entry.name = name;
        }
        entry.last_opened = self.last_opened.filter(|stamp| !stamp.is_empty());
        entry.board = self.board.filter(|board| !board.is_empty());
        entry.shield = self.shield.filter(|shield| !shield.is_empty());
        Some(entry)
    }
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

/// The config directory the *process environment* names:
/// `$XDG_CONFIG_HOME`, or `<home>/.config` when XDG names nothing absolute.
///
/// Read once, at startup, and carried as a value from there on
/// ([`crate::App::set_home_dir`] replaces it wholesale). Consulting the
/// variable on every lookup instead would overrule a redirected home ---
/// which is precisely what tests redirect it for, so an inherited
/// `XDG_CONFIG_HOME` would leak the developer's real config into fixtures.
pub fn default_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| config_dir_in(home))
}

/// The XDG fallback on its own: the config directory a given home implies.
/// Separate from [`default_config_dir`] because the callers that *redirect*
/// the home ([`crate::App::set_home_dir`] and its fixtures) need this
/// convention without the environment overruling it --- an inherited
/// `$XDG_CONFIG_HOME` is precisely what they are escaping.
pub fn config_dir_in(home: &Path) -> PathBuf {
    home.join(".config")
}

/// The user config's location inside a resolved config directory. This is
/// the path the unresolved-workspace pane tells the user about, so it must
/// be the one the app actually reads.
pub fn user_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join("chiptui/config.toml")
}

/// Reads the user config, if it exists. A missing file is `None` --- "not
/// configured yet", never an error (`SPEC.md` §13's project levels stay
/// optional).
pub fn load_user(config_dir: &Path) -> Option<ZephyrSettings> {
    let text = std::fs::read_to_string(user_config_path(config_dir)).ok()?;
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
    save_key(config, "zephyr", key, value)
}

/// Remembers where the project creator's directory picker last landed
/// (`[projects] last_parent`), so creating a second project starts where the
/// first one was put instead of at `$HOME` again.
pub fn save_last_parent(config: &Path, dir: &Path) -> std::io::Result<()> {
    save_key(
        config,
        "projects",
        "last_parent",
        &dir.display().to_string(),
    )
}

/// Reads `[projects] last_parent`, with `~` resolved against `home`.
pub fn last_parent(config_dir: &Path, home: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(user_config_path(config_dir)).ok()?;
    section_value(&text, "projects", "last_parent").map(|value| expand_home(&value, home))
}

fn save_key(config: &Path, section: &str, key: &str, value: &str) -> std::io::Result<()> {
    let text = std::fs::read_to_string(config).unwrap_or_default();
    let updated = upsert_key(&text, section, key, value);
    write_config(config, &updated)
}

/// One key from one `[section]`, for the settings that do not deserve a
/// struct of their own. Same tolerance as the parsers above: comments,
/// quoting and spacing are all optional.
fn section_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = name.trim() == section;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((found, value)) = line.split_once('=')
            && found.trim() == key
        {
            let value = unquote(value.trim());
            return (!value.is_empty()).then_some(value);
        }
    }
    None
}

/// Records `entry` in the user config: inserted, or replaced in place when
/// the path is already known (keeping the recorded name unless the caller
/// supplies a different one).
///
/// This runs on every project open, so it is also where the registry is
/// pruned: entries whose directory no longer exists are dropped, since the
/// home screen would refuse to open them anyway ([`ProjectRegistry::listed`]).
/// Everything outside the `[[project]]` blocks --- `[zephyr]`, comments,
/// unknown sections --- is preserved byte for byte; the blocks themselves
/// are machine-managed and get rewritten wholesale, so a comment *inside*
/// one does not survive.
pub fn record_project(config: &Path, entry: ProjectEntry) -> std::io::Result<()> {
    let text = std::fs::read_to_string(config).unwrap_or_default();
    let (other, mut entries) = split_projects(&text);
    entries.retain(|existing| existing.path == entry.path || existing.path.is_dir());
    match entries
        .iter_mut()
        .find(|existing| existing.path == entry.path)
    {
        Some(existing) => *existing = entry,
        None => entries.push(entry),
    }
    write_config(config, &render_projects(&other, &entries))
}

/// Removes `path` from the registry (the home screen's `d`). The directory
/// itself is never touched --- this forgets a project, it does not delete
/// one.
pub fn forget_project(config: &Path, path: &Path) -> std::io::Result<()> {
    let text = std::fs::read_to_string(config).unwrap_or_default();
    let (other, mut entries) = split_projects(&text);
    entries.retain(|entry| entry.path != path);
    write_config(config, &render_projects(&other, &entries))
}

/// Writes the user config atomically: a temporary file beside it, then a
/// rename. The file now carries the environment *and* the whole project
/// registry, and it is rewritten on every project open --- a truncated write
/// would lose both at once. Falls back to a direct write when the rename
/// fails (a filesystem without atomic rename should still get the update).
fn write_config(config: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = config.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = config.with_extension(format!("toml.tmp{}", std::process::id()));
    std::fs::write(&temp, text)?;
    if std::fs::rename(&temp, config).is_err() {
        let _ = std::fs::remove_file(&temp);
        return std::fs::write(config, text);
    }
    Ok(())
}

/// Splits `text` into everything that is not a `[[project]]` block and the
/// entries those blocks describe --- the pure half of [`record_project`].
fn split_projects(text: &str) -> (String, Vec<ProjectEntry>) {
    let is_header = |line: &str| line.split('#').next().unwrap_or("").trim().starts_with('[');
    let mut other = Vec::new();
    let mut in_project = false;
    for line in text.lines() {
        if is_header(line) {
            in_project = line.split('#').next().unwrap_or("").trim() == "[[project]]";
        }
        if !in_project {
            other.push(line.to_string());
        }
    }
    (other.join("\n"), ProjectRegistry::parse(text).entries)
}

/// Re-renders the config from its non-registry half plus `entries`.
fn render_projects(other: &str, entries: &[ProjectEntry]) -> String {
    let mut out = other.trim_end().to_string();
    for entry in entries {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("[[project]]\n");
        out.push_str(&format!(
            "path = {}\n",
            quote(&entry.path.display().to_string())
        ));
        out.push_str(&format!("backend = {}\n", quote(entry.backend.id())));
        out.push_str(&format!("name = {}\n", quote(&entry.name)));
        if let Some(board) = &entry.board {
            out.push_str(&format!("board = {}\n", quote(board)));
        }
        if let Some(shield) = &entry.shield {
            out.push_str(&format!("shield = {}\n", quote(shield)));
        }
        if let Some(stamp) = &entry.last_opened {
            out.push_str(&format!("last_opened = {}\n", quote(stamp)));
        }
        // The block's own trailing newline is the separator above.
        out.pop();
    }
    out.push('\n');
    out
}

/// The pure half of [`save_workspace`]/[`save_projects`]/[`save_last_parent`],
/// so the merge is testable without touching the filesystem.
///
/// Only the one line is touched. The section a line belongs to is tracked
/// through *every* header, not just the one being written --- otherwise a
/// key of the same name in a later section (or inside a `[[project]]`
/// block) would be the one replaced.
fn upsert_key(text: &str, section: &str, key: &str, value: &str) -> String {
    let header_name = |line: &str| {
        line.split('#')
            .next()
            .unwrap_or("")
            .trim()
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
            .map(|name| name.trim().trim_matches(['[', ']']).trim().to_string())
    };

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut in_section = false;
    let mut replaced = false;
    for line in &mut lines {
        if let Some(name) = header_name(line) {
            in_section = name == section;
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
        let header = lines
            .iter()
            .position(|line| header_name(line).is_some_and(|name| name == section));
        match header {
            Some(index) => lines.insert(index + 1, format!("{key} = \"{value}\"")),
            None => {
                lines.push(String::new());
                lines.push(format!("[{section}]"));
                lines.push(format!("{key} = \"{value}\""));
            }
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
        let updated = upsert_key(existing, "zephyr", "workspace", "/opt/myzephyr");
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
        let updated = upsert_key(
            "[zephyr]\nsdk = \"~/sdk\"\n",
            "zephyr",
            "workspace",
            "/opt/myzephyr",
        );
        assert_eq!(
            updated,
            "[zephyr]\nworkspace = \"/opt/myzephyr\"\nsdk = \"~/sdk\"\n"
        );
    }

    #[test]
    fn save_appends_the_section_when_absent() {
        let updated = upsert_key(
            "[ui]\nmouse = false\n",
            "zephyr",
            "workspace",
            "/opt/myzephyr",
        );
        assert!(updated.contains("[ui]\nmouse = false"));
        assert!(updated.ends_with("[zephyr]\nworkspace = \"/opt/myzephyr\"\n"));
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_project_blocks_and_skips_incomplete_ones() {
        let text = "[zephyr]\nworkspace = \"/ws\"\n\n\
                    [[project]]\npath = \"/p/blinky\"\nbackend = \"zephyr\"\nname = \"blinky\"\n\
                    last_opened = \"2026-08-16T14:03:11Z\"\n\n\
                    [[project]]\npath = \"/p/sensor\"\nbackend = \"micropython\"\n\n\
                    [[project]]\npath = \"/p/future\"\nbackend = \"esp-idf\"\n\n\
                    [[project]]\nbackend = \"zephyr\"\n";
        let registry = ProjectRegistry::parse(text);

        let entries = registry.entries();
        assert_eq!(entries.len(), 2, "unknown backend and missing path skipped");
        assert_eq!(entries[0].path, PathBuf::from("/p/blinky"));
        assert_eq!(entries[0].backend, BackendKind::Zephyr);
        assert_eq!(
            entries[0].last_opened.as_deref(),
            Some("2026-08-16T14:03:11Z")
        );
        assert_eq!(
            entries[1].name, "sensor",
            "a block without a name is named after its directory"
        );
        assert_eq!(
            registry.backend_for(Path::new("/p/sensor")),
            Some(BackendKind::MicroPython)
        );
        assert_eq!(registry.backend_for(Path::new("/p")), None, "exact match");
    }

    #[test]
    fn the_zephyr_section_survives_project_blocks_and_the_reverse() {
        let text =
            "[[project]]\npath = \"/p/a\"\nbackend = \"zephyr\"\n\n[zephyr]\nworkspace = \"/ws\"\n";
        assert_eq!(
            ZephyrSettings::parse(text).workspace.as_deref(),
            Some("/ws"),
            "a [[project]] block must not swallow the section after it"
        );
        assert_eq!(ProjectRegistry::parse(text).entries().len(), 1);
    }

    #[test]
    fn listed_orders_by_recency_then_name_and_hides_missing_directories() {
        let dir = temp_dir("registry-listed");
        let old = dir.join("old");
        let recent = dir.join("recent");
        let unstamped = dir.join("unstamped");
        for path in [&old, &recent, &unstamped] {
            std::fs::create_dir_all(path).unwrap();
        }
        let text = format!(
            "[[project]]\npath = \"{}\"\nbackend = \"zephyr\"\nlast_opened = \"2026-01-01T00:00:00Z\"\n\n\
             [[project]]\npath = \"{}\"\nbackend = \"micropython\"\n\n\
             [[project]]\npath = \"{}\"\nbackend = \"zephyr\"\nlast_opened = \"2026-08-16T14:03:11Z\"\n\n\
             [[project]]\npath = \"{}\"\nbackend = \"zephyr\"\nlast_opened = \"2026-08-17T00:00:00Z\"\n",
            old.display(),
            unstamped.display(),
            recent.display(),
            dir.join("vanished").display(),
        );

        let registry = ProjectRegistry::parse(&text);
        let names: Vec<&str> = registry
            .listed()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            names,
            vec!["recent", "old", "unstamped"],
            "most recent first, never-opened last, vanished omitted"
        );
    }

    #[test]
    fn record_project_upserts_prunes_and_keeps_the_rest_of_the_file() {
        let dir = temp_dir("registry-record");
        let config = dir.join("chiptui/config.toml");
        let alive = dir.join("alive");
        let other = dir.join("other");
        std::fs::create_dir_all(&alive).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(dir.join("chiptui")).unwrap();
        std::fs::write(
            &config,
            format!(
                "# my setup\n[zephyr]\nworkspace = \"/ws\"\n\n\
                 [[project]]\npath = \"{}\"\nbackend = \"zephyr\"\nname = \"alive\"\n\n\
                 [[project]]\npath = \"{}\"\nbackend = \"zephyr\"\nname = \"gone\"\n",
                alive.display(),
                dir.join("gone").display(),
            ),
        )
        .unwrap();

        record_project(
            &config,
            ProjectEntry::new(&other, BackendKind::MicroPython).opened_now(),
        )
        .unwrap();
        // The same path again: replaced in place, not duplicated.
        record_project(&config, ProjectEntry::new(&alive, BackendKind::Zephyr)).unwrap();

        let text = std::fs::read_to_string(&config).unwrap();
        let registry = ProjectRegistry::parse(&text);
        let paths: Vec<&Path> = registry
            .entries()
            .iter()
            .map(|entry| entry.path.as_path())
            .collect();
        let zephyr = ZephyrSettings::parse(&text);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(paths, vec![alive.as_path(), other.as_path()]);
        assert_eq!(
            zephyr.workspace.as_deref(),
            Some("/ws"),
            "[zephyr] survives"
        );
        assert!(text.contains("# my setup"), "comments survive:\n{text}");
        assert!(
            !text.contains("gone"),
            "a vanished project is pruned:\n{text}"
        );
    }

    #[test]
    fn forget_project_removes_one_entry_and_leaves_the_directory_alone() {
        let dir = temp_dir("registry-forget");
        let config = dir.join("config.toml");
        let kept = dir.join("kept");
        let dropped = dir.join("dropped");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::create_dir_all(&dropped).unwrap();
        record_project(&config, ProjectEntry::new(&kept, BackendKind::Zephyr)).unwrap();
        record_project(&config, ProjectEntry::new(&dropped, BackendKind::Zephyr)).unwrap();

        forget_project(&config, &dropped).unwrap();

        let registry = ProjectRegistry::parse(&std::fs::read_to_string(&config).unwrap());
        let still_there = dropped.is_dir();
        let paths: Vec<PathBuf> = registry
            .entries()
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(paths, vec![kept]);
        assert!(still_there, "forgetting a project never deletes it");
    }

    #[test]
    fn a_recorded_path_round_trips_through_the_writer() {
        let entry = ProjectEntry::new("/p/with \"quotes\" and \\slash", BackendKind::Zephyr);
        let text = render_projects("", std::slice::from_ref(&entry));
        assert_eq!(ProjectRegistry::parse(&text).entries(), &[entry]);
    }

    #[test]
    fn board_and_shield_answers_round_trip_through_the_registry() {
        let mut entry = ProjectEntry::new("/apps/blinky", BackendKind::Zephyr);
        entry.board = Some("nrf52840dk/nrf52840".to_string());
        entry.shield = Some("nrf7002ek".to_string());
        let text = render_projects("", std::slice::from_ref(&entry));
        assert!(
            text.contains("board = \"nrf52840dk/nrf52840\""),
            "the board is written as its own key:\n{text}"
        );
        assert_eq!(ProjectRegistry::parse(&text).entries()[0], entry);

        // Clearing the shield (and changing the board) rewrites the same
        // entry in place: the old answers do not linger.
        entry.board = Some("thingy91/nrf9160".to_string());
        entry.shield = None;
        let rewritten = render_projects("", std::slice::from_ref(&entry));
        assert!(
            !rewritten.contains("nrf7002ek"),
            "a cleared shield leaves no line behind:\n{rewritten}"
        );
        assert_eq!(ProjectRegistry::parse(&rewritten).entries()[0], entry);
    }

    #[test]
    fn hand_written_board_and_shield_keys_are_read() {
        let registry = ProjectRegistry::parse(
            "[[project]]\npath = \"/apps/blinky\"\nbackend = \"zephyr\"\n\
             board = 'nrf52840dk/nrf52840'\nshield = \"nrf7002ek\"\n",
        );
        let entry = &registry.entries()[0];
        assert_eq!(entry.board.as_deref(), Some("nrf52840dk/nrf52840"));
        assert_eq!(entry.shield.as_deref(), Some("nrf7002ek"));
    }

    #[test]
    fn load_expands_home_in_recorded_paths() {
        let dir = temp_dir("registry-home");
        let config_dir = dir.join(".config");
        std::fs::create_dir_all(config_dir.join("chiptui")).unwrap();
        std::fs::write(
            user_config_path(&config_dir),
            "[[project]]\npath = \"~/apps/blinky\"\nbackend = \"zephyr\"\n",
        )
        .unwrap();

        let registry = ProjectRegistry::load(&config_dir, &dir);
        let path = registry.entries()[0].path.clone();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(path, dir.join("apps/blinky"));
    }

    #[test]
    fn save_projects_touches_only_the_projects_line() {
        let existing = "[zephyr]\nworkspace = \"/ws\"\nprojects = \"/old\"\n";
        let updated = upsert_key(existing, "zephyr", "projects", "/opt/apps");
        assert_eq!(
            updated,
            "[zephyr]\nworkspace = \"/ws\"\nprojects = \"/opt/apps\"\n"
        );
    }
}
