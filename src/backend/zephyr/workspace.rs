//! Locating the west workspace and turning it into a west invocation.
//!
//! A Zephyr development environment has three pieces, and only the first is
//! required knowledge:
//!
//! 1. the **west workspace** (`~/zephyrproject` by convention): the
//!    directory `west init` created, holding `.west/`, the Zephyr checkout
//!    and, by the same convention, `.venv/`;
//! 2. the **venv** where `west` (and only west, for this app's purposes) is
//!    installed;
//! 3. the **Zephyr SDK** (toolchain), which CMake finds on its own in the
//!    usual locations unless `ZEPHYR_SDK_INSTALL_DIR` names one.
//!
//! Everything here answers one question --- *which `west`, in which
//! environment, against which workspace?* --- with an explainable source for
//! every answer (`AGENTS.md` §4's "detection must be explainable", applied to
//! the environment instead of the project). An application living *outside*
//! the workspace (`~/zephyrprojects/app1` against `~/zephyrproject`) works
//! because every command carries `ZEPHYR_BASE`: west's documented fallback
//! for finding the workspace when the `.west/` walk-up from the cwd fails.
//!
//! No venv activation is attempted or needed: the venv's `west` console
//! script embeds the absolute path of the venv's interpreter in its shebang,
//! so executing it directly *is* the activated environment. What `activate`
//! would additionally provide, this reproduces per command: `PATH` with the
//! venv's `bin` first and `VIRTUAL_ENV` set. Nothing is exported into
//! ChipTUI's own process.

use std::path::{Path, PathBuf};

use crate::process::Command;
use crate::settings::{ZephyrSettings, expand_home};

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

/// Where a workspace answer came from --- the workspace pane's one-word hint,
/// and the priority order for defaulting a picker (config beats proximity
/// beats environment beats convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceOrigin {
    /// `chiptui.toml`'s `[zephyr] workspace` key.
    ProjectConfig,
    /// The user config's (`~/.config/chiptui/config.toml`) `[zephyr]
    /// workspace` key.
    UserConfig,
    /// A `.west/` directory found walking up from the project.
    InProject,
    /// `$ZEPHYR_BASE` exported into ChipTUI's own environment.
    EnvVar,
    /// `~/zephyrproject`, the getting-started convention.
    HomeDefault,
    /// Chosen in the workspace picker, for this session only.
    Picked,
}

impl WorkspaceOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::ProjectConfig => "chiptui.toml",
            Self::UserConfig => "user config",
            Self::InProject => "project is inside it",
            Self::EnvVar => "$ZEPHYR_BASE",
            Self::HomeDefault => "~/zephyrproject",
            Self::Picked => "picked (this session)",
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
    /// The toolchain location when one is known (config key or inherited
    /// `ZEPHYR_SDK_INSTALL_DIR`); `None` means CMake's own discovery.
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
}

/// Everything [`resolve`] needs, with the environment pieces passed in
/// rather than read from the process so tests stay deterministic and
/// parallel-safe.
#[derive(Debug, Clone)]
pub struct ResolveInput<'a> {
    pub project_root: &'a Path,
    /// `[zephyr]` from the project's `chiptui.toml`, if present.
    pub project_settings: Option<&'a ZephyrSettings>,
    /// `[zephyr]` from the user config, if present.
    pub user_settings: Option<&'a ZephyrSettings>,
    /// `$ZEPHYR_BASE` inherited from the shell that started ChipTUI.
    pub zephyr_base_env: Option<&'a Path>,
    pub home: &'a Path,
}

/// The outcome of resolving the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// One answer --- explicit config, or the only candidate found.
    Single(Workspace),
    /// Several candidates (best first): the workspace picker's rows.
    Ambiguous(Vec<Workspace>),
    /// A configured workspace that does not look like one; the message is
    /// the actionable explanation (`SPEC.md` §14).
    Invalid(String),
    /// Nothing found; the pane explains the two config levels.
    Missing,
}

/// Resolves the workspace: explicit config first (project over user ---
/// a project pinned to a second checkout must not fight the machine's
/// default), then discovery, which asks whenever it finds more than one
/// candidate rather than guessing (`SPEC.md` §7's detection philosophy,
/// applied to the environment).
pub fn resolve(input: &ResolveInput<'_>) -> Resolution {
    if let Some(settings) = input.project_settings.filter(|s| s.workspace.is_some()) {
        return configured(input, settings, WorkspaceOrigin::ProjectConfig);
    }
    if let Some(settings) = input.user_settings.filter(|s| s.workspace.is_some()) {
        return configured(input, settings, WorkspaceOrigin::UserConfig);
    }

    let mut candidates: Vec<Workspace> = Vec::new();
    let push = |workspace: Option<Workspace>, candidates: &mut Vec<Workspace>| {
        if let Some(workspace) = workspace
            && !candidates.iter().any(|w| w.dir == workspace.dir)
        {
            candidates.push(workspace);
        }
    };

    push(
        workspace_above(input.project_root)
            .map(|dir| from_dir(input, dir, WorkspaceOrigin::InProject)),
        &mut candidates,
    );
    push(
        input
            .zephyr_base_env
            .and_then(workspace_for_zephyr_base)
            .map(|dir| from_dir(input, dir, WorkspaceOrigin::EnvVar)),
        &mut candidates,
    );
    push(
        Some(input.home.join("zephyrproject"))
            .filter(|dir| is_workspace(dir))
            .map(|dir| from_dir(input, dir, WorkspaceOrigin::HomeDefault)),
        &mut candidates,
    );

    match candidates.len() {
        0 => Resolution::Missing,
        1 => Resolution::Single(candidates.remove(0)),
        _ => Resolution::Ambiguous(candidates),
    }
}

/// The explicit-config path: one answer, no question --- but validated, so a
/// typo in the config surfaces as an explanation instead of west's least
/// helpful failure later.
fn configured(
    input: &ResolveInput<'_>,
    settings: &ZephyrSettings,
    origin: WorkspaceOrigin,
) -> Resolution {
    let raw = settings.workspace.as_deref().unwrap_or_default();
    let dir = expand_home(raw, input.home);
    if !is_workspace(&dir) {
        return Resolution::Invalid(format!(
            "the workspace configured in the {} is {} --- it has no .west/ directory",
            match origin {
                WorkspaceOrigin::ProjectConfig => "project's chiptui.toml",
                _ => "user config",
            },
            dir.display()
        ));
    }
    Resolution::Single(from_settings(input, dir, origin, settings))
}

/// Builds the [`Workspace`] for `dir` found by discovery (no explicit
/// settings to honor beyond any the *user* config still carries for sdk/west
/// --- a configured toolchain stays useful even when the workspace itself is
/// discovered).
fn from_dir(input: &ResolveInput<'_>, dir: PathBuf, origin: WorkspaceOrigin) -> Workspace {
    let settings = input
        .project_settings
        .or(input.user_settings)
        .cloned()
        .unwrap_or_default();
    from_settings(input, dir, origin, &settings)
}

/// Builds the [`Workspace`] for a validated `dir`, layering the explicit
/// pieces (west/sdk) from `settings` over the conventional derivations.
fn from_settings(
    input: &ResolveInput<'_>,
    dir: PathBuf,
    origin: WorkspaceOrigin,
    settings: &ZephyrSettings,
) -> Workspace {
    let manifest_path = manifest_path(&dir);
    let zephyr_base = dir.join(&manifest_path);
    let venv = dir.join(".venv").is_dir().then(|| dir.join(".venv"));
    let west = if let Some(west) = settings.west.as_deref() {
        expand_home(west, input.home).display().to_string()
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
        .map(|sdk| expand_home(sdk, input.home))
        .or_else(|| std::env::var_os("ZEPHYR_SDK_INSTALL_DIR").map(PathBuf::from));
    Workspace {
        dir,
        origin,
        zephyr_base,
        venv,
        west,
        sdk,
    }
}

/// Whether `dir` is a west workspace: west's own marker is the `.west`
/// directory, whose parent is the workspace root.
fn is_workspace(dir: &Path) -> bool {
    dir.join(".west").is_dir()
}

/// Walks `start` and its ancestors for the first `.west/` directory,
/// returning the workspace root above it. An application checked out
/// anywhere inside the workspace (a sample, `zephyr/tests/...`) resolves to
/// that workspace.
fn workspace_above(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| is_workspace(dir))
        .map(Path::to_path_buf)
}

/// Maps a `$ZEPHYR_BASE` value to its workspace: normally
/// `<workspace>/zephyr`, so the parent; a value pointing at the workspace
/// root itself also works (someone exported the imprecise spelling, and the
/// marker is unambiguous).
fn workspace_for_zephyr_base(base: &Path) -> Option<PathBuf> {
    let parent = base.parent()?;
    if is_workspace(parent) {
        return Some(parent.to_path_buf());
    }
    is_workspace(base).then(|| base.to_path_buf())
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

    /// Builds a throwaway workspace: `.west/config`, `zephyr/VERSION`,
    /// optionally `.venv/bin/west`.
    fn workspace_dir(root: &Path, name: &str, with_venv: bool) -> PathBuf {
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

    fn input<'a>(root: &'a Path, home: &'a Path) -> ResolveInput<'a> {
        ResolveInput {
            project_root: root,
            project_settings: None,
            user_settings: None,
            zephyr_base_env: None,
            home,
        }
    }

    #[test]
    fn a_workspace_above_the_project_wins_discovery() {
        let tmp = scratch("above");
        let ws = workspace_dir(&tmp, "zephyrproject", false);
        let app = ws.join("myapp");
        std::fs::create_dir_all(&app).unwrap();

        let Resolution::Single(workspace) = resolve(&input(&app, &tmp)) else {
            panic!("expected a single candidate");
        };
        assert_eq!(workspace.dir, ws);
        assert_eq!(workspace.origin, WorkspaceOrigin::InProject);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn zephyr_base_env_resolves_to_its_parent_workspace() {
        let tmp = scratch("env");
        let ws = workspace_dir(&tmp, "zephyrproject", false);
        let app = tmp.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let mut input = input(&app, &tmp);
        let zephyr_base = ws.join("zephyr");
        input.zephyr_base_env = Some(&zephyr_base);

        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a single candidate");
        };
        assert_eq!(workspace.dir, ws);
        assert_eq!(workspace.origin, WorkspaceOrigin::EnvVar);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn two_candidates_ask_rather_than_guess() {
        let tmp = scratch("two");
        let ws = workspace_dir(&tmp, "zephyrproject", false);
        let nested = workspace_dir(&tmp, "otherproject", false);
        let app = nested.join("app");
        std::fs::create_dir_all(&app).unwrap();

        let Resolution::Ambiguous(candidates) = resolve(&input(&app, &tmp)) else {
            panic!("expected the picker");
        };
        // The project's own workspace leads: it is the stronger signal.
        assert_eq!(candidates[0].dir, nested);
        assert!(candidates.iter().any(|w| w.dir == ws));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn nothing_found_is_missing() {
        let tmp = scratch("none");
        let app = tmp.join("app");
        std::fs::create_dir_all(&app).unwrap();
        assert_eq!(resolve(&input(&app, &tmp)), Resolution::Missing);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn explicit_config_beats_discovery_and_project_beats_user() {
        let tmp = scratch("config");
        let ws = workspace_dir(&tmp, "configured", false);
        let home_default = workspace_dir(&tmp, "zephyrproject", false);
        let app = tmp.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let home = home_default.clone();

        let user = ZephyrSettings {
            workspace: Some(home_default.display().to_string()),
            ..Default::default()
        };
        let project = ZephyrSettings {
            workspace: Some(ws.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&app, &home);
        input.user_settings = Some(&user);
        input.project_settings = Some(&project);

        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("explicit config never asks");
        };
        assert_eq!(workspace.dir, ws, "project config outranks user config");
        assert_eq!(workspace.origin, WorkspaceOrigin::ProjectConfig);

        input.project_settings = None;
        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("explicit config never asks");
        };
        assert_eq!(workspace.dir, home_default);
        assert_eq!(workspace.origin, WorkspaceOrigin::UserConfig);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_configured_workspace_without_west_is_reported_not_guessed_around() {
        let tmp = scratch("invalid");
        let app = tmp.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let settings = ZephyrSettings {
            workspace: Some(tmp.join("nope").display().to_string()),
            ..Default::default()
        };
        let mut input = input(&app, &tmp);
        input.user_settings = Some(&settings);
        let Resolution::Invalid(message) = resolve(&input) else {
            panic!("expected an invalid report");
        };
        assert!(
            message.contains(".west"),
            "message explains the marker: {message}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_venv_west_is_preferred_over_the_path() {
        let tmp = scratch("venv");
        let ws = workspace_dir(&tmp, "zephyrproject", true);
        let app = tmp.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let mut input = input(&app, &tmp);
        let zephyr_base = ws.join("zephyr");
        input.zephyr_base_env = Some(&zephyr_base);

        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a single candidate");
        };
        assert_eq!(
            workspace.west,
            ws.join(".venv/bin/west").display().to_string()
        );

        let west_env = workspace.to_env("/usr/bin:/bin");
        assert_eq!(
            west_env.program,
            ws.join(".venv/bin/west").display().to_string()
        );
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
        let ws = workspace_dir(&tmp, "zephyrproject", false);
        let workspace = from_dir(&input(&tmp, &tmp), ws, WorkspaceOrigin::HomeDefault);
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
        let ws = workspace_dir(&tmp, "zephyrproject", false);
        let settings = ZephyrSettings {
            workspace: Some(ws.display().to_string()),
            sdk: Some("~/zephyr-sdk-0.17.1".to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp, &tmp);
        input.project_settings = Some(&settings);
        let Resolution::Single(workspace) = resolve(&input) else {
            panic!("expected a single candidate");
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
        let ws = workspace_dir(&tmp, "zephyrproject", false);
        std::fs::write(ws.join(".west/config"), "[manifest]\npath = zephyr-fork\n").unwrap();
        std::fs::create_dir_all(ws.join("zephyr-fork")).unwrap();
        let workspace = from_dir(&input(&tmp, &tmp), ws, WorkspaceOrigin::HomeDefault);
        assert_eq!(workspace.zephyr_base, workspace.dir.join("zephyr-fork"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_zephyr_version_reads_the_checkout_file() {
        let tmp = scratch("version");
        let ws = workspace_dir(&tmp, "zephyrproject", false);
        let workspace = from_dir(&input(&tmp, &tmp), ws, WorkspaceOrigin::HomeDefault);
        assert_eq!(workspace.zephyr_version().as_deref(), Some("4.1"));

        std::fs::write(
            workspace.zephyr_base.join("VERSION"),
            "VERSION_MAJOR = 3\nVERSION_MINOR = 7\nPATCHLEVEL = 4\n",
        )
        .unwrap();
        assert_eq!(workspace.zephyr_version().as_deref(), Some("3.7.4"));
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
        assert_eq!(
            command.to_string(),
            "ZEPHYR_BASE=/ws/zephyr /ws/.venv/bin/west build"
        );
    }
}
