//! Locating the Zephyr installation and turning it into a west invocation.
//!
//! A Zephyr development environment has three pieces, and only the first is
//! required knowledge:
//!
//! 1. the **west workspace** (`west init`'s directory: `.west/`, the Zephyr
//!    checkout, and by convention `.venv/`) --- what the getting-started
//!    guide builds;
//! 2. the **venv** where `west` (and only west, for this app's purposes) is
//!    installed;
//! 3. the **Zephyr SDK** (toolchain), which CMake finds on its own in the
//!    usual locations unless `sdk` names one.
//!
//! The location comes from configuration and nowhere else --- no directory
//! conventions, no inherited environment variables. When no config names
//! it, the UI asks with a real directory picker (the user knows where their
//! installation lives; guessing is `SPEC.md` §8's cardinal sin, applied to
//! the environment). Whatever the config or the picker says is then
//! *validated*: a directory without `.west/` is not an installation, and
//! the answer is the getting-started guide, not a silent west failure later.
//!
//! An application living *outside* the workspace (`~/zephyrprojects/app1`
//! against the installation elsewhere) works because every command carries
//! `ZEPHYR_BASE`: west's documented fallback for finding the workspace when
//! the `.west/` walk-up from the cwd fails. That variable is *derived* here
//! and injected per command --- the user never sets it.
//!
//! No venv activation is performed or needed: the venv's `west` console
//! script embeds the absolute path of the venv's interpreter in its
//! shebang, so executing it directly *is* the activated environment. What
//! `activate` would additionally provide, this reproduces per command:
//! `PATH` with the venv's `bin` first and `VIRTUAL_ENV` set. Nothing is
//! exported into ChipTUI's own process.

use std::path::{Path, PathBuf};

use crate::process::Command;
use crate::settings::{ZephyrSettings, expand_home};

/// The installation guide the validation errors point to: when a directory
/// is not a Zephyr installation, the fix is installing Zephyr there (or
/// pointing at the real installation), and this is the instructions.
pub const GETTING_STARTED: &str =
    "https://docs.zephyrproject.org/latest/develop/getting_started/index.html";

/// The west invocation derived from a [`Workspace`]: the executable to run
/// and the environment overrides every command carries. Applying it is a
/// decoration over commands built by [`super::commands`], the same way the
/// project root (`cwd`) is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WestEnv {
    /// The west executable: the venv's console script when one exists, an
    /// explicit config override, or bare `west` (a `PATH` lookup, the
    /// pre-workspace behavior).
    pub program: String,
    /// Environment overrides, applied to every command.
    pub env: Vec<(String, String)>,
}

impl WestEnv {
    /// The pre-workspace invocation: `west` from `PATH`, no overrides.
    pub fn from_path() -> Self {
        Self {
            program: super::commands::PROGRAM.to_string(),
            env: Vec::new(),
        }
    }

    /// Decorates a command constructed by [`super::commands`] with the
    /// resolved executable and environment.
    pub fn apply(&self, command: Command) -> Command {
        command.with_program(&self.program).envs(self.env.clone())
    }
}

/// Where a workspace answer came from --- the workspace pane's one-word
/// hint. Only config sources exist: a picker choice is *written to* one of
/// them before it counts, so there is no third "picked" origin that could
/// disagree with the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceOrigin {
    /// `chiptui.toml`'s `[zephyr] workspace` key.
    ProjectConfig,
    /// The user config's (`~/.config/chiptui/config.toml`) `[zephyr]
    /// workspace` key.
    UserConfig,
}

impl WorkspaceOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::ProjectConfig => "chiptui.toml",
            Self::UserConfig => "user config",
        }
    }
}

/// A resolved west workspace with everything derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The workspace root (the parent of `.west/`).
    pub dir: PathBuf,
    pub origin: WorkspaceOrigin,
    /// `<workspace>/<manifest.path>` (`.west/config`'s `[manifest] path`,
    /// `zephyr` unless a manifest moved it). Exported as `ZEPHYR_BASE` on
    /// every command.
    pub zephyr_base: PathBuf,
    /// The venv directory when `<workspace>/.venv` exists.
    pub venv: Option<PathBuf>,
    /// The resolved west executable (see [`WestEnv::program`]).
    pub west: String,
    /// The toolchain location when `sdk` configured one; `None` means
    /// CMake's own discovery.
    pub sdk: Option<PathBuf>,
}

impl Workspace {
    /// The west invocation for this workspace.
    pub fn to_env(&self, inherited_path: &str) -> WestEnv {
        let mut env = vec![(
            "ZEPHYR_BASE".to_string(),
            self.zephyr_base.display().to_string(),
        )];
        if let Some(sdk) = &self.sdk {
            env.push((
                "ZEPHYR_SDK_INSTALL_DIR".to_string(),
                sdk.display().to_string(),
            ));
        }
        if let Some(venv) = &self.venv {
            env.push(("VIRTUAL_ENV".to_string(), venv.display().to_string()));
            env.push((
                "PATH".to_string(),
                format!("{}/bin:{inherited_path}", venv.display()),
            ));
        }
        WestEnv {
            program: self.west.clone(),
            env,
        }
    }

    /// The tools whose *location* this workspace owns, for
    /// [`crate::backend::BackendRegistry::tool_status`]: judging them
    /// against `PATH` would call a perfectly good venv west "missing" for
    /// never having been exported. Resolution's own fallback --- a bare
    /// program name --- is not a location, so it is reported as absent here
    /// and the `PATH` answer stands, which is what `west = "west"` (and a
    /// venv with no west installed into it) asks for.
    pub fn tool_locations(&self) -> Vec<(&'static str, PathBuf)> {
        let west = Path::new(&self.west);
        if is_bare_name(west) {
            return Vec::new();
        }
        vec![(super::commands::PROGRAM, west.to_path_buf())]
    }

    /// The checkout's version, read from `zephyr/VERSION` (a file west
    /// already keeps exact --- no subprocess for a fact the workspace owns).
    pub fn zephyr_version(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.zephyr_base.join("VERSION")).ok()?;
        let field = |name: &str| {
            text.lines().find_map(|line| {
                let value = line.trim().strip_prefix(name)?.trim_start();
                let value = value.strip_prefix('=')?.trim();
                (!value.is_empty()).then(|| value.to_string())
            })
        };
        let major = field("VERSION_MAJOR")?;
        let minor = field("VERSION_MINOR").unwrap_or_default();
        let patch = field("PATCHLEVEL").unwrap_or_default();
        let mut version = format!("{major}.{minor}");
        if !patch.is_empty() && patch != "0" {
            version.push_str(&format!(".{patch}"));
        }
        Some(version)
    }

    /// The toolchain's version, read from the SDK's `sdk_version` file when
    /// an SDK location is known.
    pub fn sdk_version(&self) -> Option<String> {
        let sdk = self.sdk.as_ref()?;
        std::fs::read_to_string(sdk.join("sdk_version"))
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
    }

    /// The venv's Python version, read from `<venv>/pyvenv.cfg`'s
    /// `version =` line --- the file the venv itself owns, so no
    /// subprocess is needed for a fact it already records. `None`
    /// without a venv or a readable file.
    pub fn python_version(&self) -> Option<String> {
        let venv = self.venv.as_ref()?;
        let text = std::fs::read_to_string(venv.join("pyvenv.cfg")).ok()?;
        text.lines().find_map(|line| {
            let value = line.trim().strip_prefix("version")?.trim_start();
            let value = value.strip_prefix('=')?.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
    }
}

/// Everything resolution needs, with the config levels passed in rather
/// than read from the process so tests stay deterministic and
/// parallel-safe.
#[derive(Debug, Clone)]
pub struct ResolveInput<'a> {
    /// `[zephyr]` from the project's `chiptui.toml`, if present.
    pub project_settings: Option<&'a ZephyrSettings>,
    /// `[zephyr]` from the user config, if present.
    pub user_settings: Option<&'a ZephyrSettings>,
    pub home: &'a Path,
}

/// The outcome of resolving the installation location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A validated installation.
    Single(Workspace),
    /// A configured location that is not one; the message is the
    /// actionable explanation, including the install guide (`SPEC.md` §14).
    Invalid(String),
    /// No config names a location: the pane prompts, the picker decides.
    NotConfigured,
}

/// Resolves the installation location: the project's config first (a
/// project pinned to a second installation must not fight the machine's
/// default), then the user config. Nothing else --- when neither file
/// names a location, the answer is "ask the user", never a guess.
pub fn resolve(input: &ResolveInput<'_>) -> Resolution {
    if let Some(settings) = input.project_settings.filter(|s| s.workspace.is_some()) {
        return check(input, settings, WorkspaceOrigin::ProjectConfig);
    }
    if let Some(settings) = input.user_settings.filter(|s| s.workspace.is_some()) {
        return check(input, settings, WorkspaceOrigin::UserConfig);
    }
    Resolution::NotConfigured
}

/// Validates the location one config level names, and builds the
/// [`Workspace`] when it is a real installation.
fn check(
    input: &ResolveInput<'_>,
    settings: &ZephyrSettings,
    origin: WorkspaceOrigin,
) -> Resolution {
    let dir = expand_home(
        settings.workspace.as_deref().unwrap_or_default(),
        input.home,
    );
    install_check(input, dir, origin, settings)
}

/// Validates that `dir` is a completed getting-started installation and
/// builds the workspace when it is. Public because the directory picker
/// validates a *user-chosen* directory through the exact same rules the
/// config goes through --- one definition of "installed here", two doors.
pub fn install_check(
    input: &ResolveInput<'_>,
    dir: PathBuf,
    origin: WorkspaceOrigin,
    settings: &ZephyrSettings,
) -> Resolution {
    match install_state(&dir) {
        InstallState::Absent => Resolution::Invalid(format!(
            "{} is not a Zephyr installation (no .west/ directory) — install guide: {GETTING_STARTED}",
            dir.display()
        )),
        InstallState::Partial => Resolution::Invalid(format!(
            "{} is a west workspace but has no {} checkout (west update?) — install guide: {GETTING_STARTED}",
            dir.display(),
            manifest_path(&dir)
        )),
        InstallState::Complete => Resolution::Single(from_settings(input, dir, origin, settings)),
    }
}

/// How far along a directory is towards being a Zephyr installation.
///
/// The two predicates [`install_check`] judges by, without the message or
/// the [`Workspace`] it builds --- the installer needs the same answer to
/// word its offer ("install", "finish", "use") and to decide whether it has
/// anything to run at all. One definition of "is this an installation",
/// shared, rather than a second copy of `.west/`-and-checkout next door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    /// Nothing here: no `.west/`.
    Absent,
    /// `west init` ran but the manifest was never checked out --- an
    /// interrupted `west update`, which is resumable.
    Partial,
    /// A usable installation.
    Complete,
}

pub fn install_state(dir: &Path) -> InstallState {
    if !dir.join(".west").is_dir() {
        return InstallState::Absent;
    }
    if !dir.join(manifest_path(dir)).is_dir() {
        return InstallState::Partial;
    }
    InstallState::Complete
}

/// Builds the [`Workspace`] for a validated `dir`, layering the explicit
/// pieces (west/sdk) from `settings` over the conventional derivations.
fn from_settings(
    input: &ResolveInput<'_>,
    dir: PathBuf,
    origin: WorkspaceOrigin,
    settings: &ZephyrSettings,
) -> Workspace {
    let zephyr_base = dir.join(manifest_path(&dir));
    let venv = dir.join(".venv").is_dir().then(|| dir.join(".venv"));
    let west = if let Some(west) = settings.west.as_deref() {
        // A configured *path* is anchored to the workspace, never to the
        // cwd: the cwd of the process and the cwd of the commands are two
        // different directories (a picked project re-roots the latter), so
        // a relative override would be validated against one and executed
        // against the other. `join` leaves an absolute path alone. A bare
        // program name carries no directory and stays a `PATH` lookup ---
        // `west = "west"` asks for exactly that.
        let west = expand_home(west, input.home);
        if is_bare_name(&west) {
            west
        } else {
            dir.join(west)
        }
        .display()
        .to_string()
    } else if let Some(venv) = &venv
        && venv.join("bin/west").is_file()
    {
        venv.join("bin/west").display().to_string()
    } else {
        super::commands::PROGRAM.to_string()
    };
    let sdk = settings
        .sdk
        .as_deref()
        .map(|sdk| expand_home(sdk, input.home));
    Workspace {
        dir,
        origin,
        zephyr_base,
        venv,
        west,
        sdk,
    }
}

/// Whether a configured program is a bare name --- no directory component
/// at all, which means `PATH` decides where it comes from, exactly as it
/// does for every tool nobody configured. Anything with a directory in it
/// is a *location*, and gets treated as one.
fn is_bare_name(program: &Path) -> bool {
    program
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
}

/// Reads `.west/config`'s `[manifest] path` key (west keeps the file in
/// configparser form). The default, and the value for every stock manifest,
/// is `zephyr`.
fn manifest_path(workspace: &Path) -> String {
    std::fs::read_to_string(workspace.join(".west/config"))
        .ok()
        .and_then(|text| {
            let mut in_section = false;
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    in_section = name.trim() == "manifest";
                    continue;
                }
                if !in_section {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=')
                    && key.trim() == "path"
                {
                    let value = value.trim().trim_matches('"');
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
            None
        })
        .unwrap_or_else(|| "zephyr".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a throwaway installation: `.west/config`, `zephyr/VERSION`,
    /// optionally `.venv/bin/west`.
    fn install_dir(root: &Path, name: &str, with_venv: bool) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join(".west")).unwrap();
        std::fs::create_dir_all(dir.join("zephyr")).unwrap();
        std::fs::write(dir.join(".west/config"), "[manifest]\npath = zephyr\n").unwrap();
        std::fs::write(
            dir.join("zephyr/VERSION"),
            "VERSION_MAJOR = 4\nVERSION_MINOR = 1\nPATCHLEVEL = 0\n",
        )
        .unwrap();
        if with_venv {
            std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
            std::fs::write(dir.join(".venv/bin/west"), "#!/bin/sh\n").unwrap();
        }
        dir
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chiptui-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn input<'a>(home: &'a Path) -> ResolveInput<'a> {
        ResolveInput {
            project_settings: None,
            user_settings: None,
            home,
        }
    }

    #[test]
    fn a_configured_location_is_validated_and_resolved() {
        let tmp = scratch("configured");
        let ws = install_dir(&tmp, "myzephyr", true);
        let settings = ZephyrSettings {
            workspace: Some(ws.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.project_settings = Some(&settings);

        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a resolved installation");
        };
        assert_eq!(workspace.dir, ws);
        assert_eq!(workspace.origin, WorkspaceOrigin::ProjectConfig);
        assert_eq!(workspace.zephyr_version().as_deref(), Some("4.1"));
        // The venv's Python comes from pyvenv.cfg, never a subprocess.
        assert_eq!(workspace.python_version(), None, "no pyvenv.cfg yet");
        std::fs::write(
            ws.join(".venv/pyvenv.cfg"),
            "home = /usr/bin\nversion = 3.12.4\n",
        )
        .unwrap();
        assert_eq!(workspace.python_version().as_deref(), Some("3.12.4"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A `west` override is a location in the workspace, not in whatever
    /// directory ChipTUI happened to start in --- the commands run with the
    /// project root as cwd, so a cwd-relative answer would be checked in one
    /// place and executed in another. A bare name stays a `PATH` lookup.
    #[test]
    fn a_west_override_is_anchored_to_the_workspace() {
        let tmp = scratch("westpath");
        let ws = install_dir(&tmp, "myzephyr", false);
        let resolved = |west: &str| {
            let settings = ZephyrSettings {
                workspace: Some(ws.display().to_string()),
                west: Some(west.to_string()),
                ..Default::default()
            };
            let mut input = input(&tmp);
            input.user_settings = Some(&settings);
            let Resolution::Single(workspace) = resolve(&input) else {
                panic!("expected a resolved installation");
            };
            workspace.west
        };

        assert_eq!(
            resolved("tools/west"),
            ws.join("tools/west").display().to_string()
        );
        assert_eq!(resolved("/opt/west"), "/opt/west");
        assert_eq!(
            resolved(super::super::commands::PROGRAM),
            super::super::commands::PROGRAM,
            "a bare name asks for PATH, and must not become <workspace>/west"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn without_any_config_the_answer_is_ask_not_guess() {
        let tmp = scratch("none");
        assert_eq!(resolve(&input(&tmp)), Resolution::NotConfigured);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_location_without_west_is_rejected_with_the_guide() {
        let tmp = scratch("nowest");
        let somewhere = tmp.join("somewhere");
        std::fs::create_dir_all(&somewhere).unwrap();
        let settings = ZephyrSettings {
            workspace: Some(somewhere.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.user_settings = Some(&settings);
        let Resolution::Invalid(message) = resolve(&input) else {
            panic!("expected a rejection");
        };
        assert!(message.contains(".west"), "names the marker: {message}");
        assert!(
            message.contains(GETTING_STARTED),
            "points at the install guide: {message}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_workspace_without_a_checkout_is_rejected_too() {
        let tmp = scratch("nocheckout");
        let ws = tmp.join("half");
        std::fs::create_dir_all(ws.join(".west")).unwrap();
        std::fs::write(ws.join(".west/config"), "[manifest]\npath = zephyr\n").unwrap();
        let settings = ZephyrSettings {
            workspace: Some(ws.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.user_settings = Some(&settings);
        let Resolution::Invalid(message) = resolve(&input) else {
            panic!("expected a rejection");
        };
        assert!(
            message.contains("west update"),
            "says what is missing: {message}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_picker_validates_through_the_same_rules() {
        let tmp = scratch("picker");
        let ws = install_dir(&tmp, "myzephyr", false);
        let settings = ZephyrSettings::default();
        let Resolution::Single(workspace) = install_check(
            &input(&tmp),
            ws.clone(),
            WorkspaceOrigin::UserConfig,
            &settings,
        ) else {
            panic!("a real installation passes");
        };
        assert_eq!(workspace.dir, ws);
        assert_eq!(workspace.origin, WorkspaceOrigin::UserConfig);

        let Resolution::Invalid(message) = install_check(
            &input(&tmp),
            tmp.clone(),
            WorkspaceOrigin::UserConfig,
            &settings,
        ) else {
            panic!("the scratch dir is not an installation");
        };
        assert!(message.contains(GETTING_STARTED));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn project_config_outranks_user_config() {
        let tmp = scratch("prio");
        let project_ws = install_dir(&tmp, "project-pinned", false);
        let user_ws = install_dir(&tmp, "machine-default", false);
        let user = ZephyrSettings {
            workspace: Some(user_ws.display().to_string()),
            ..Default::default()
        };
        let project = ZephyrSettings {
            workspace: Some(project_ws.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.user_settings = Some(&user);
        input.project_settings = Some(&project);
        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a resolved installation");
        };
        assert_eq!(workspace.dir, project_ws);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_venv_west_is_preferred_over_the_path() {
        let tmp = scratch("venv");
        let ws = install_dir(&tmp, "myzephyr", true);
        let settings = ZephyrSettings {
            workspace: Some(ws.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.user_settings = Some(&settings);
        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a resolved installation");
        };
        assert_eq!(
            workspace.west,
            ws.join(".venv/bin/west").display().to_string()
        );

        let west_env = workspace.to_env("/usr/bin:/bin");
        let get = |key: &str| {
            west_env
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("ZEPHYR_BASE"), ws.join("zephyr").display().to_string());
        assert_eq!(
            get("PATH"),
            format!("{}/bin:/usr/bin:/bin", ws.join(".venv").display())
        );
        assert_eq!(get("VIRTUAL_ENV"), ws.join(".venv").display().to_string());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn without_a_venv_west_comes_from_the_path() {
        let tmp = scratch("nvenv");
        let ws = install_dir(&tmp, "myzephyr", false);
        let settings = ZephyrSettings {
            workspace: Some(ws.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.user_settings = Some(&settings);
        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a resolved installation");
        };
        assert_eq!(workspace.west, "west");
        let west_env = workspace.to_env("/usr/bin");
        assert!(
            west_env.env.iter().all(|(k, _)| k != "PATH"),
            "nothing to prepend"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_configured_sdk_becomes_the_env_var() {
        let tmp = scratch("sdk");
        let ws = install_dir(&tmp, "myzephyr", false);
        let settings = ZephyrSettings {
            workspace: Some(ws.display().to_string()),
            sdk: Some("~/zephyr-sdk-0.17.1".to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.project_settings = Some(&settings);
        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a resolved installation");
        };
        assert_eq!(
            workspace.sdk.as_deref(),
            Some(tmp.join("zephyr-sdk-0.17.1").as_path())
        );
        let west_env = workspace.to_env("");
        assert!(west_env.env.contains(&(
            "ZEPHYR_SDK_INSTALL_DIR".to_string(),
            tmp.join("zephyr-sdk-0.17.1").display().to_string()
        )));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn manifest_path_moves_zephyr_base() {
        let tmp = scratch("manifest");
        let ws = install_dir(&tmp, "myzephyr", false);
        std::fs::write(ws.join(".west/config"), "[manifest]\npath = zephyr-fork\n").unwrap();
        std::fs::create_dir_all(ws.join("zephyr-fork")).unwrap();
        let settings = ZephyrSettings {
            workspace: Some(ws.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.user_settings = Some(&settings);
        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a resolved installation");
        };
        assert_eq!(workspace.zephyr_base, workspace.dir.join("zephyr-fork"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn west_env_applies_program_and_environment_to_a_command() {
        let west_env = WestEnv {
            program: "/ws/.venv/bin/west".to_string(),
            env: vec![("ZEPHYR_BASE".to_string(), "/ws/zephyr".to_string())],
        };
        let command = west_env.apply(crate::process::Command::new("west").arg("build"));
        assert_eq!(command.program(), "/ws/.venv/bin/west");
        assert_eq!(command.to_string(), "west build");
    }
}
