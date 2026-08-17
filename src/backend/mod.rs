//! Backend abstraction.
//!
//! A backend answers three questions and nothing else for now:
//!
//! 1. *Is this directory a project of my kind?* --- [`Backend::detect`] returns
//!    weighted evidence, never a boolean, so detection stays explainable.
//! 2. *What can be done with such a project?* --- [`Backend::capabilities`].
//! 3. *Which command runs a supported operation?* --- [`Backend::monitor_command`]
//!    and [`Backend::build_command`], both optional (a backend that offers no
//!    such operation, or has not implemented it yet, returns `None`).
//!
//! The UI consumes capabilities; it never asks "is this MicroPython?".
//! Operations stay behind small optional trait methods rather than a wider
//! operations trait: there is exactly one caller shape per operation today,
//! and `AGENTS.md` §8 asks for no abstraction without a concrete use case.

pub mod micropython;
pub mod registry;
pub mod zephyr;

use std::fmt;

use ratatui::style::Color;
use ratatui_themes::ThemePalette;

use crate::project::{DirScan, Signal};

pub use registry::BackendRegistry;

/// The backends that exist today. Adding a variant is a deliberate act; the UI
/// never matches on it to decide which actions to offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BackendKind {
    MicroPython,
    Zephyr,
}

impl BackendKind {
    pub const ALL: &'static [BackendKind] = &[BackendKind::MicroPython, BackendKind::Zephyr];

    /// Stable identifier used by configuration and manual overrides.
    pub const fn id(self) -> &'static str {
        match self {
            Self::MicroPython => "micropython",
            Self::Zephyr => "zephyr",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::MicroPython => "MicroPython",
            Self::Zephyr => "Zephyr",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.id() == id)
    }

    /// The backend's mark in a list, next to its name.
    pub const fn icon(self) -> &'static str {
        match self {
            Self::MicroPython => "🐍",
            Self::Zephyr => "🔷",
        }
    }

    /// How the backend colors a row it owns (`SPEC.md` §11's home screen).
    ///
    /// Presentation identity, alongside [`Self::display_name`] and
    /// [`Self::icon`] --- not behavior. The rule that keeps the UI free of
    /// backend conditionals is about *capabilities*; a list that shows two
    /// backends at once still has to tell them apart at a glance, and the
    /// backend is the only place that can say how.
    ///
    /// The colors are derived from the *active theme*: each backend claims
    /// one of the theme's semantic colors as its mark (MicroPython the green
    /// `success`, Zephyr the blue `info`), and the row tints are that color
    /// blended toward the theme's own background, so a theme switch recolors
    /// the rows too --- a light theme gets light tints --- instead of
    /// leaving them pinned to fixed 256-color indexes.
    pub const fn palette(self, theme: ThemePalette) -> Palette {
        match self {
            Self::MicroPython => Palette {
                accent: theme.success,
                tint: blend(theme.success, theme.bg, 3),
                tint_selected: blend(theme.success, theme.bg, 6),
            },
            Self::Zephyr => Palette {
                accent: theme.info,
                tint: blend(theme.info, theme.bg, 3),
                tint_selected: blend(theme.info, theme.bg, 6),
            },
        }
    }
}

/// `color` moved `num`/16 of the way toward `background` (an integer lerp,
/// so it stays `const`). Non-RGB colors cannot be blended; the theme's
/// background is returned instead --- an untinted row --- rather than a
/// guess. Every `ratatui_themes` palette uses RGB values, so this only
/// guards custom construction.
const fn blend(color: Color, background: Color, num: u32) -> Color {
    if let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (color, background) {
        Color::Rgb(
            channel(r1, r2, num),
            channel(g1, g2, num),
            channel(b1, b2, num),
        )
    } else {
        background
    }
}

/// One channel of [`blend`]: `from` shifted `num`/16 toward `toward`.
const fn channel(toward: u8, from: u8, num: u32) -> u8 {
    let delta = toward as i32 - from as i32;
    (from as i32 + delta * num as i32 / 16) as u8
}

/// A backend's colors: the accent its name and icon are drawn in, and the
/// two background tints a row of its own uses --- resting and selected ---
/// both derived from the active theme by [`BackendKind::palette`], so the
/// row is tinted rather than painted and the text on it stays legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub tint: Color,
    pub tint_selected: Color,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Which flavor of the build lifecycle a [`Backend::build_command`] asks
/// for. Kept as one enum because the panel offers them as one list; the
/// backend decides what each one maps to (`west build`, `west build -t
/// clean`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    /// Incremental build; configures the first time when needed.
    Build,
    /// Removes build artifacts.
    Clean,
    /// Discards the build directory and builds from scratch.
    Rebuild,
}

impl BuildKind {
    /// The lifecycle in its own order: clean clears the way, build makes,
    /// rebuild makes again from scratch. Also the panel's row order.
    pub const ALL: &'static [BuildKind] = &[BuildKind::Clean, BuildKind::Build, BuildKind::Rebuild];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Build => "Build",
            Self::Clean => "Clean",
            Self::Rebuild => "Rebuild",
        }
    }
}

/// An operation a backend may support.
///
/// Kept flat and small on purpose: each variant must correspond to something
/// the UI can actually render as an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Capability {
    Build,
    Clean,
    Flash,
    EraseFlash,
    Upload,
    Download,
    Filesystem,
    Repl,
    Monitor,
    Run,
    Reset,
    DeviceInfo,
    PackageInstall,
    BoardSelect,
    /// The backend's targets can carry an optional add-on board (a Zephyr
    /// shield): the UI offers a shield picker beside the board one, and the
    /// answer is saved with the board pick.
    ShieldSelect,
    /// The backend's projects live in a user-chosen folder rather than
    /// necessarily the working directory: the UI offers a projects-folder
    /// setting and a project picker, and gates project commands (build,
    /// clean, ...) on a selected, buildable project.
    ProjectSelect,
    /// Maintaining the backend's shared environment (`west update`,
    /// `west sdk list`): operations that act on the workspace rather than
    /// the project. Not destructive as a capability --- the pane confirms
    /// the state-changing action itself.
    WorkspaceSync,
}

impl Capability {
    pub const ALL: &'static [Capability] = &[
        Capability::Build,
        Capability::Clean,
        Capability::Flash,
        Capability::EraseFlash,
        Capability::Upload,
        Capability::Download,
        Capability::Filesystem,
        Capability::Repl,
        Capability::Monitor,
        Capability::Run,
        Capability::Reset,
        Capability::DeviceInfo,
        Capability::PackageInstall,
        Capability::BoardSelect,
        Capability::ShieldSelect,
        Capability::ProjectSelect,
        Capability::WorkspaceSync,
    ];

    const fn bit(self) -> u32 {
        1u32 << (self as u32)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Clean => "clean",
            Self::Flash => "flash",
            Self::EraseFlash => "erase flash",
            Self::Upload => "upload",
            Self::Download => "download",
            Self::Filesystem => "filesystem",
            Self::Repl => "repl",
            Self::Monitor => "monitor",
            Self::Run => "run",
            Self::Reset => "reset",
            Self::DeviceInfo => "device info",
            Self::PackageInstall => "install package",
            Self::BoardSelect => "select board",
            Self::ShieldSelect => "select shield",
            Self::ProjectSelect => "select project",
            Self::WorkspaceSync => "sync workspace",
        }
    }

    /// Whether invoking this operation can destroy user data or device state.
    ///
    /// `SPEC.md` §15: these always require confirmation. Marking it on the
    /// capability keeps the rule in one place instead of in every view.
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Flash | Self::EraseFlash | Self::Clean)
    }
}

/// A set of [`Capability`] values, stored as a bitmask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities(u32);

impl Capabilities {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn from_slice(caps: &[Capability]) -> Self {
        let mut bits = 0u32;
        let mut i = 0;
        while i < caps.len() {
            bits |= caps[i].bit();
            i += 1;
        }
        Self(bits)
    }

    pub const fn with(self, cap: Capability) -> Self {
        Self(self.0 | cap.bit())
    }

    pub const fn contains(self, cap: Capability) -> bool {
        self.0 & cap.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Supported capabilities, in [`Capability::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL
            .iter()
            .copied()
            .filter(move |cap| self.contains(*cap))
    }

    /// Supported capabilities that require confirmation before running.
    pub fn destructive(self) -> impl Iterator<Item = Capability> {
        self.iter().filter(|cap| cap.is_destructive())
    }
}

impl FromIterator<Capability> for Capabilities {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        iter.into_iter().fold(Self::empty(), Self::with)
    }
}

/// A framework-specific backend.
pub trait Backend {
    fn kind(&self) -> BackendKind;

    /// Weighted evidence that `scan` is a project of this backend's kind.
    ///
    /// Returning an empty vector means "no evidence", not "not this kind".
    fn detect(&self, scan: &DirScan) -> Vec<Signal>;

    /// Total signal weight at which detection is considered fully confident.
    ///
    /// Confidence is `min(1, score / saturation)`, which keeps the number
    /// explainable: "3.0 of the 4.0 points needed".
    fn saturation(&self) -> f32;

    fn capabilities(&self) -> Capabilities;

    /// External executables this backend delegates to (`AGENTS.md` §2).
    fn required_tools(&self) -> &'static [&'static str];

    /// The layout a new project of this kind starts with (`SPEC.md` §7),
    /// for a project directory named `name`. Empty by default: a backend
    /// that has nothing to lay down leaves the directory as it found it.
    ///
    /// This is the backend's own answer, exactly like [`Self::detect`] ---
    /// the code that writes it ([`crate::project::scaffold::create`]) never
    /// asks which backend it is dealing with.
    fn scaffold(&self, name: &str) -> crate::project::Scaffold {
        let _ = name;
        crate::project::Scaffold::default()
    }

    /// Returns the command to launch an interactive serial monitor/REPL.
    /// Returns `None` if the backend doesn't support a monitor, or if it isn't implemented.
    fn monitor_command(&self, port: Option<&str>) -> Option<crate::process::Command> {
        let _ = port;
        None
    }

    /// Returns the command for one flavor of the build lifecycle
    /// (`AGENTS.md` §2: delegate to the ecosystem's own tools). `board` is
    /// the target the backend should configure for, when one is known;
    /// `shield` the optional add-on board riding on it (`None` means no
    /// shield, and must lead to no shield flag at all); `build_dir_exists`
    /// lets an incremental build skip the flags only a first configuration
    /// needs; `build_dir` names the directory the lifecycle targets (a
    /// backend with a single fixed directory may ignore it). Returns `None`
    /// if the backend offers no build capability or has not implemented it
    /// yet.
    fn build_command(
        &self,
        kind: BuildKind,
        board: Option<&str>,
        shield: Option<&str>,
        build_dir_exists: bool,
        build_dir: &str,
    ) -> Option<crate::process::Command> {
        let _ = (kind, board, shield, build_dir_exists, build_dir);
        None
    }

    /// Returns the command listing the board targets this backend can build
    /// for (`west boards`). Returns `None` if the backend has no board
    /// selection ([`Capability::BoardSelect`]) or has not implemented it.
    /// The command may be slow; callers run it in the background.
    fn board_list_command(&self) -> Option<crate::process::Command> {
        None
    }

    /// Returns the command listing the shields the backend's targets can
    /// carry (`west shields`). Returns `None` if the backend has no shield
    /// selection ([`Capability::ShieldSelect`]) or has not implemented it.
    fn shield_list_command(&self) -> Option<crate::process::Command> {
        None
    }

    /// Returns the command that writes the built image to the device.
    /// Returns `None` if the backend has no [`Capability::Flash`] single
    /// command --- MicroPython's flashing is a multi-step esptool flow the
    /// Flash dialog owns, so it stays `None` there.
    fn flash_command(&self, build_dir: &str) -> Option<crate::process::Command> {
        let _ = build_dir;
        None
    }

    /// Returns the interactive configuration command over the configured
    /// build directory (`west build -t menuconfig`). The caller runs it with
    /// the terminal suspended, not through the piped process manager.
    /// Returns `None` when the backend has no such tool.
    fn menuconfig_command(&self, build_dir: &str) -> Option<crate::process::Command> {
        let _ = build_dir;
        None
    }

    /// Returns the command syncing the backend's shared environment with
    /// its manifest (`west update`) --- a workspace-wide, slow, state-
    /// changing operation the caller confirms before running. Returns
    /// `None` when the backend has no [`Capability::WorkspaceSync`].
    fn workspace_update_command(&self) -> Option<crate::process::Command> {
        None
    }

    /// Returns the read-only command listing the toolchain inventory
    /// (`west sdk list`). Returns `None` when the backend has no
    /// [`Capability::WorkspaceSync`].
    fn sdk_list_command(&self) -> Option<crate::process::Command> {
        None
    }
}

/// Whether `name` resolves to an executable on `PATH`.
///
/// Used to report a missing toolchain as an actionable error rather than
/// letting the first command fail with "No such file or directory".
pub fn tool_available(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    lookup_on_path(name, &path)
}

/// The pure half of [`tool_available`], with `PATH` passed in --- mutating
/// the process environment would make these tests parallel-hostile.
fn lookup_on_path(name: &str, path: &std::ffi::OsStr) -> bool {
    std::env::split_paths(path)
        // An *empty* `PATH` entry (a `::`, or a leading/trailing `:`) means
        // the current directory, to `split_paths` and to `execvp` alike ---
        // `PathBuf::from("").join("west")` is just `west`. Reporting a
        // binary that happens to sit in whatever project directory ChipTUI
        // was started from as *the system tool* is a claim this cannot back,
        // so those entries are skipped. Note the scope: this makes the
        // report stricter than execution, never the reverse. Spawning still
        // hands the bare program name to `execvp`, which reads the
        // unfiltered `PATH` --- refusing to run it would belong at the spawn
        // layer, not here.
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| executable_at(&dir.join(name)))
}

/// Whether `path` names a file that can actually be executed --- the one
/// definition of "runnable", shared by the `PATH` lookup above and by
/// callers holding an explicit location (a configured tool, a venv's
/// console script). Existence alone is not enough: a checkout without the
/// permission bit, or a `west.py` named as the executable, fails at spawn
/// with `Permission denied` long after the UI called it available.
///
/// One `stat` answers both halves: this runs once per `PATH` entry per
/// tool, so asking the filesystem twice for the same metadata is pure
/// waste.
pub fn executable_at(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && is_executable(&meta))
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file is not a tool: `west` without the permission bit (a checkout
    /// that lost it, or a `west.py` named as the executable) would be
    /// reported as available and then fail at spawn.
    #[test]
    fn availability_needs_the_execute_bit() {
        let dir = std::env::temp_dir().join(format!("chiptui-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("faketool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();

        assert!(!executable_at(&tool), "no execute bit, no tool");
        assert!(!lookup_on_path("faketool", dir.as_os_str()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(executable_at(&tool));
            assert!(lookup_on_path("faketool", dir.as_os_str()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `split_paths` turns a `::` or a trailing `:` into an empty entry,
    /// which joins to a bare relative path --- the cwd. The fixture below is
    /// executable and lives at a known path relative to the package root
    /// (the cwd of a test binary), so honoring the empty entry would find it.
    #[test]
    fn an_empty_path_entry_is_not_the_current_directory() {
        let relative = std::path::Path::new("tests/fixtures/bin/mpremote");
        assert!(
            executable_at(relative),
            "the fixture must be executable for this test to prove anything"
        );
        let name = relative.to_str().unwrap();
        assert!(!lookup_on_path(name, std::ffi::OsStr::new("")));
        assert!(!lookup_on_path(name, std::ffi::OsStr::new("/nonexistent:")));
    }

    #[test]
    fn capability_bits_are_distinct() {
        let mut seen = Vec::new();
        for cap in Capability::ALL {
            assert!(!seen.contains(&cap.bit()), "duplicate bit for {cap:?}");
            seen.push(cap.bit());
        }
        // The bitmask is a u32; the enum must not outgrow it.
        assert!(Capability::ALL.len() <= 32);
    }

    #[test]
    fn set_round_trips_through_iter() {
        let caps =
            Capabilities::from_slice(&[Capability::Build, Capability::Monitor, Capability::Flash]);
        assert_eq!(caps.len(), 3);
        assert_eq!(
            caps.iter().collect::<Vec<_>>(),
            // iteration order follows Capability::ALL, not insertion order
            vec![Capability::Build, Capability::Flash, Capability::Monitor]
        );
    }

    #[test]
    fn contains_reports_only_declared_capabilities() {
        let caps = Capabilities::from_slice(&[Capability::Repl]);
        assert!(caps.contains(Capability::Repl));
        assert!(!caps.contains(Capability::Build));
        assert!(!Capabilities::empty().contains(Capability::Repl));
        assert!(Capabilities::empty().is_empty());
    }

    #[test]
    fn adding_a_capability_twice_is_idempotent() {
        let once = Capabilities::empty().with(Capability::Flash);
        assert_eq!(once.with(Capability::Flash), once);
        assert_eq!(once.len(), 1);
    }

    #[test]
    fn from_iter_matches_from_slice() {
        let caps = [Capability::Clean, Capability::Build];
        assert_eq!(
            caps.into_iter().collect::<Capabilities>(),
            Capabilities::from_slice(&caps)
        );
    }

    #[test]
    fn destructive_capabilities_require_confirmation() {
        // SPEC.md §15: erase, flash and clean remove state the user cannot recover.
        assert!(Capability::EraseFlash.is_destructive());
        assert!(Capability::Flash.is_destructive());
        assert!(Capability::Clean.is_destructive());
        assert!(!Capability::Monitor.is_destructive());
        assert!(!Capability::DeviceInfo.is_destructive());

        let caps = Capabilities::from_slice(&[Capability::Monitor, Capability::EraseFlash]);
        assert_eq!(
            caps.destructive().collect::<Vec<_>>(),
            vec![Capability::EraseFlash]
        );
    }

    #[test]
    fn backend_kind_ids_round_trip() {
        for kind in BackendKind::ALL {
            assert_eq!(BackendKind::from_id(kind.id()), Some(*kind));
        }
        assert_eq!(BackendKind::from_id("esp-idf"), None);
    }
}
