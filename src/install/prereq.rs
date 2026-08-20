//! What the machine must already have before Zephyr can be installed.
//!
//! ChipTUI installs **nothing** system-wide. This module's whole job is to
//! *report*: for each prerequisite it runs the tool's own `--version`, parses
//! the answer ([`super::version`]), compares it against Zephyr's documented
//! minimum, and --- when the answer is "missing" or "too old" --- names the
//! command that would fix it for whichever package managers this machine
//! actually has. The user runs that command; the checklist re-checks on `r`.
//!
//! The minimums come from the Zephyr getting-started page's requirements
//! table. Only three of the four rows *block* the installation: `python3`
//! is reported for information, because the installer pins its own
//! interpreter through pyenv (see [`super::steps`]) and the system's Python
//! never reaches the build. Blocking on a row the installer itself exists
//! to satisfy would be a checkbox no one could tick.

use std::path::PathBuf;

use crate::process::Command;

use super::version::Version;

/// Zephyr's minimum CMake (getting-started requirements table).
pub const CMAKE_MIN: Version = Version::new(3, 28, 0);
/// Zephyr's minimum device tree compiler.
pub const DTC_MIN: Version = Version::new(1, 4, 6);
/// The Python series Zephyr recommends --- and the one pyenv is pointed at.
pub const PYTHON_SERIES: Version = Version::new(3, 12, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prereq {
    Cmake,
    Dtc,
    Pyenv,
    Python,
}

impl Prereq {
    /// Checked in the order the checklist lists them: the two that gate the
    /// *build*, then the one that gates the *installation*, then the
    /// informational one.
    pub const ALL: &'static [Prereq] = &[Prereq::Cmake, Prereq::Dtc, Prereq::Pyenv, Prereq::Python];

    pub const fn program(self) -> &'static str {
        match self {
            Self::Cmake => "cmake",
            Self::Dtc => "dtc",
            Self::Pyenv => "pyenv",
            Self::Python => "python3",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Cmake => "cmake",
            Self::Dtc => "dtc",
            Self::Pyenv => "pyenv",
            Self::Python => "python",
        }
    }

    /// The version this row demands, when it demands one. `pyenv` has no
    /// minimum (any release can install a 3.12), and `python3` is measured
    /// against a *series* rather than a floor --- see [`Probe::classify`].
    pub const fn minimum(self) -> Option<Version> {
        match self {
            Self::Cmake => Some(CMAKE_MIN),
            Self::Dtc => Some(DTC_MIN),
            Self::Pyenv | Self::Python => None,
        }
    }

    /// Whether a failing answer stops the installation. Everything but the
    /// system Python does: see the module docs.
    pub const fn blocking(self) -> bool {
        !matches!(self, Self::Python)
    }

    /// The version query. `dtc` is the odd one --- it prints its banner on
    /// stderr with `--version`, which the process manager captures all the
    /// same, so no special casing is needed here.
    pub fn query(self) -> Command {
        Command::new(self.program()).arg("--version")
    }

    /// Where to get it, when it is missing or too old: one line per package
    /// manager this machine actually has, so the suggestion is runnable
    /// rather than a menu of other distributions. With no manager
    /// recognised, the upstream page is the honest answer.
    ///
    /// `pyenv` is never a distribution package worth recommending (the
    /// distro builds lag, and the whole point is picking an interpreter
    /// version), so it always gets its own installer plus the shell note.
    pub fn install_hint(self, available: impl Fn(&str) -> bool) -> Vec<String> {
        if self == Self::Pyenv {
            return vec![
                "curl -fsSL https://pyenv.run | bash".to_string(),
                "then add pyenv's bin/ to PATH in your shell rc".to_string(),
                PYENV_DOCS.to_string(),
            ];
        }
        let hints: Vec<String> = MANAGERS
            .iter()
            .filter(|(manager, _)| available(manager))
            .map(|(_, install)| format!("{install} {}", self.package()))
            .collect();
        if hints.is_empty() {
            vec![self.docs().to_string()]
        } else {
            hints
        }
    }

    /// The package name, where it differs from the program name.
    const fn package(self) -> &'static str {
        match self {
            Self::Cmake => "cmake",
            // Debian and Fedora ship dtc inside a differently named
            // package; naming the program instead would send the user to a
            // "no such package" error.
            Self::Dtc => "dtc",
            Self::Pyenv => "pyenv",
            Self::Python => "python3",
        }
    }

    const fn docs(self) -> &'static str {
        match self {
            Self::Cmake => "https://cmake.org/download/",
            Self::Dtc => "https://www.devicetree.org/",
            Self::Pyenv => PYENV_DOCS,
            Self::Python => "https://www.python.org/downloads/",
        }
    }
}

const PYENV_DOCS: &str = "https://github.com/pyenv/pyenv#installation";

/// The package managers worth suggesting, in the order a hint lists them,
/// each with the command that installs a package. Detection is
/// [`crate::backend::tool_available`] --- the same `PATH` predicate the
/// header's missing-tools badge uses --- so a machine only ever sees its own.
const MANAGERS: &[(&str, &str)] = &[
    ("pacman", "sudo pacman -S"),
    ("apt", "sudo apt install"),
    ("dnf", "sudo dnf install"),
    ("zypper", "sudo zypper install"),
    ("brew", "brew install"),
];

/// What the version query found. `Probing` is the state a row sits in
/// between the spawn and the answer --- a checklist that renders before its
/// subprocesses return must say so rather than claim "missing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Probing,
    /// Present and good enough.
    Ok(Version),
    /// Present, but below the minimum this row demands.
    Old(Version),
    /// Present, and outside the recommended series --- the Python row's
    /// only failure mode, and a warning rather than a refusal.
    OffSeries(Version),
    /// Present, but its version output could not be read. Treated as
    /// satisfied for a blocking row: refusing to install over a tool that
    /// *is* there, because it worded its banner unusually, would be the
    /// worse mistake.
    Unreadable,
    Missing,
}

impl Probe {
    /// Classifies a finished query. `found` is `None` when the program
    /// could not be started at all.
    pub fn classify(prereq: Prereq, found: Option<Version>) -> Self {
        let Some(version) = found else {
            return Self::Unreadable;
        };
        match prereq.minimum() {
            Some(minimum) if !version.at_least(minimum) => Self::Old(version),
            Some(_) => Self::Ok(version),
            None if prereq == Prereq::Python && !version.same_series(PYTHON_SERIES) => {
                Self::OffSeries(version)
            }
            None => Self::Ok(version),
        }
    }

    /// Whether this answer lets the installation start. Only a blocking
    /// row's `Missing`/`Old` stops it (`Probing` does too --- an unanswered
    /// question is not a yes).
    pub fn satisfied(&self) -> bool {
        matches!(self, Self::Ok(_) | Self::OffSeries(_) | Self::Unreadable)
    }

    /// The version to show, when one was read.
    pub fn version(&self) -> Option<Version> {
        match self {
            Self::Ok(version) | Self::Old(version) | Self::OffSeries(version) => Some(*version),
            Self::Probing | Self::Unreadable | Self::Missing => None,
        }
    }
}

/// One checklist row: the prerequisite, what the query found, and the
/// process the answer is still coming from.
#[derive(Debug, Clone)]
pub struct PrereqState {
    pub prereq: Prereq,
    pub probe: Probe,
    /// Accumulated output of the running query --- parsed at the end, like
    /// every other background list this codebase fetches.
    pub(super) output: String,
    pub(super) process: Option<crate::process::ProcessId>,
}

impl PrereqState {
    pub(super) fn new(prereq: Prereq) -> Self {
        Self {
            prereq,
            probe: Probe::Probing,
            output: String::new(),
            process: None,
        }
    }

    /// Whether this row lets the installation start: a non-blocking row
    /// always does, whatever it found.
    pub fn satisfied(&self) -> bool {
        !self.prereq.blocking() || self.probe.satisfied()
    }
}

/// The `pyenv` prefix the installer builds an interpreter path from
/// (`<root>/versions/<version>/bin/python`). Read from `pyenv root`'s
/// single line of output rather than assumed to be `~/.pyenv`, which is
/// only the default.
pub fn pyenv_root(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocking_row_refuses_only_what_it_must() {
        assert!(!Probe::classify(Prereq::Cmake, Some(Version::new(3, 20, 0))).satisfied());
        assert!(Probe::classify(Prereq::Cmake, Some(Version::new(3, 28, 0))).satisfied());
        assert!(Probe::classify(Prereq::Dtc, Some(Version::new(1, 7, 0))).satisfied());
        assert!(!Probe::classify(Prereq::Dtc, Some(Version::new(1, 4, 5))).satisfied());
        // A tool that is there but worded its banner oddly is not a refusal.
        assert!(Probe::classify(Prereq::Cmake, None).satisfied());
        assert!(!Probe::Missing.satisfied());
        // Nor is an unanswered question a yes.
        assert!(!Probe::Probing.satisfied());
    }

    #[test]
    fn the_python_row_warns_but_never_blocks() {
        let off = Probe::classify(Prereq::Python, Some(Version::new(3, 11, 9)));
        assert_eq!(off, Probe::OffSeries(Version::new(3, 11, 9)));
        // The row's own verdict is "not what Zephyr recommends"...
        let mut state = PrereqState::new(Prereq::Python);
        state.probe = Probe::Missing;
        // ...but the installation never waits on it: pyenv provides 3.12.
        assert!(state.satisfied());
        assert!(!Prereq::Python.blocking());
        assert!(Prereq::Pyenv.blocking());
    }

    #[test]
    fn pyenv_has_no_minimum_but_must_be_present() {
        assert_eq!(
            Probe::classify(Prereq::Pyenv, Some(Version::new(1, 0, 0))),
            Probe::Ok(Version::new(1, 0, 0))
        );
        let mut state = PrereqState::new(Prereq::Pyenv);
        state.probe = Probe::Missing;
        assert!(!state.satisfied());
    }

    #[test]
    fn hints_name_only_the_managers_this_machine_has() {
        let hints = Prereq::Cmake.install_hint(|manager| manager == "pacman");
        assert_eq!(hints, vec!["sudo pacman -S cmake".to_string()]);

        let hints = Prereq::Dtc.install_hint(|manager| matches!(manager, "apt" | "brew"));
        assert_eq!(
            hints,
            vec![
                "sudo apt install dtc".to_string(),
                "brew install dtc".to_string()
            ]
        );

        // No manager recognised: the upstream page, never a guessed distro.
        let hints = Prereq::Cmake.install_hint(|_| false);
        assert_eq!(hints, vec!["https://cmake.org/download/".to_string()]);
    }

    #[test]
    fn pyenv_is_always_its_own_installer() {
        let hints = Prereq::Pyenv.install_hint(|_| true);
        assert!(hints[0].contains("pyenv.run"));
        assert!(hints.iter().any(|hint| hint.contains("shell rc")));
    }

    #[test]
    fn the_queries_stay_the_tools_own() {
        assert_eq!(Prereq::Cmake.query().to_string(), "cmake --version");
        assert_eq!(Prereq::Dtc.query().to_string(), "dtc --version");
        assert_eq!(Prereq::Pyenv.query().to_string(), "pyenv --version");
        assert_eq!(Prereq::Python.query().to_string(), "python3 --version");
    }

    #[test]
    fn the_pyenv_prefix_is_read_never_assumed() {
        assert_eq!(
            pyenv_root("/home/dev/.pyenv\n"),
            Some(PathBuf::from("/home/dev/.pyenv"))
        );
        assert_eq!(pyenv_root("\n\n"), None);
    }
}
