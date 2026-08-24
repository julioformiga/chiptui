//! MicroPython backend.
//!
//! This module holds detection and capabilities. Everything that talks to a
//! board lives in the submodules: [`commands`] builds `mpremote` invocations
//! and [`parse`] reads their output. Nothing here runs a process --- the caller
//! hands the command to the [`crate::process::ProcessManager`], which keeps the
//! event loop free (`AGENTS.md` §5).

pub mod commands;
pub mod curl;
pub mod deps;
pub mod esptool;
pub mod firmware;
pub mod packages;
pub mod parse;
pub mod projects;

use crate::backend::{Backend, BackendKind, Capabilities, Capability};
use crate::project::{DirScan, Signal};

/// Files whose mere presence identifies an `mpremote`-driven project.
const MPREMOTE_CONFIG_FILES: &[&str] = &["mpremote.toml", ".mpremote.toml", "micropython.toml"];

/// Markers that turn a generic Python project into a MicroPython one.
const MICROPYTHON_MARKERS: &[&str] = &["micropython", "mpremote"];

pub struct MicroPythonBackend;

impl Backend for MicroPythonBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::MicroPython
    }

    fn detect(&self, scan: &DirScan) -> Vec<Signal> {
        let mut signals = Vec::new();

        // Firmware entry points. `boot.py` is the strongest single hint: it is
        // meaningless outside a MicroPython/CircuitPython board.
        if scan.has_file("boot.py") {
            signals.push(Signal::new("boot.py", 1.5, "boot.py at project root"));
        }
        if scan.has_file("main.py") {
            signals.push(Signal::new("main.py", 0.75, "main.py at project root"));
        }

        // Deliberately weak: it only matters combined with a real marker, and
        // it is what keeps a plain Python project below the threshold.
        if scan.has_file_with_suffix(".py") {
            signals.push(Signal::new(
                "python-sources",
                0.25,
                "Python sources at project root",
            ));
        }

        // `pyproject.toml` counts only through its *contents* --- never on its
        // own (AGENTS.md §4, SPEC.md §19).
        if MICROPYTHON_MARKERS
            .iter()
            .any(|marker| scan.text_contains("pyproject.toml", marker))
        {
            signals.push(Signal::new(
                "pyproject-micropython",
                2.5,
                "pyproject.toml references MicroPython/mpremote",
            ));
        }
        if MICROPYTHON_MARKERS
            .iter()
            .any(|marker| scan.text_contains("requirements.txt", marker))
        {
            signals.push(Signal::new(
                "requirements-micropython",
                1.0,
                "requirements.txt requires mpremote/micropython",
            ));
        }

        if scan.has_any_file(MPREMOTE_CONFIG_FILES) {
            signals.push(Signal::new(
                "mpremote-config",
                2.0,
                "mpremote configuration file",
            ));
        }
        if scan.has_file("manifest.py") {
            signals.push(Signal::new("manifest.py", 0.5, "frozen-module manifest.py"));
        }

        signals
    }

    fn saturation(&self) -> f32 {
        3.0
    }

    fn capabilities(&self) -> Capabilities {
        // SPEC.md §6: no build step --- MicroPython runs source directly.
        // `ProjectSelect` here means the same as for Zephyr: the projects
        // live in a user-chosen folder (`[micropython] projects`), so the
        // Project pane asks where and which one, and a pick re-roots the
        // file browser's local side (session-only).
        Capabilities::from_slice(&[
            Capability::Upload,
            Capability::Download,
            Capability::Filesystem,
            Capability::Repl,
            Capability::Monitor,
            Capability::Run,
            Capability::Reset,
            Capability::DeviceInfo,
            Capability::PackageInstall,
            Capability::Flash,
            Capability::EraseFlash,
            Capability::ProjectSelect,
        ])
    }

    fn required_tools(&self) -> &'static [&'static str] {
        &["mpremote", "esptool"]
    }

    /// `src/` holds what is kept in sync with the device and `firmware/`
    /// receives downloaded images --- the two directories the file browser
    /// and the firmware view open (`SPEC.md` §9). The two entry points go in
    /// with them, because the device runs `boot.py` then `main.py` by name:
    /// an empty `src/` would leave the user guessing at a convention the
    /// board already has.
    fn scaffold(&self, name: &str) -> crate::project::Scaffold {
        crate::project::Scaffold {
            dirs: vec!["src".into(), "firmware".into()],
            files: vec![
                crate::project::ScaffoldFile::new(
                    "src/boot.py",
                    "# Runs once on power-on and on every reset, before main.py.\n\
                     # Network, filesystem and peripheral setup belongs here.\n",
                ),
                crate::project::ScaffoldFile::new(
                    "src/main.py",
                    format!(
                        "# {name}: runs after boot.py, and again after every soft reset.\n\
                         import time\n\
                         \n\
                         \n\
                         def main():\n\
                         \x20   while True:\n\
                         \x20       print(\"hello from {name}\")\n\
                         \x20       time.sleep(1)\n\
                         \n\
                         \n\
                         if __name__ == \"__main__\":\n\
                         \x20   main()\n"
                    ),
                ),
                // The Dependencies row's file starts in the project from the
                // first minute: an empty-but-present requirements.txt is the
                // answer the row reads, and the header documents the grammar.
                crate::project::ScaffoldFile::new(
                    "requirements.txt",
                    crate::backend::micropython::deps::REQUIREMENTS_TEMPLATE,
                ),
            ],
        }
    }

    fn monitor_command(&self, port: Option<&str>) -> Option<crate::process::Command> {
        Some(commands::repl(port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weight(scan: &DirScan) -> f32 {
        MicroPythonBackend
            .detect(scan)
            .iter()
            .map(|s| s.weight)
            .sum()
    }

    #[test]
    fn declares_no_build_capability() {
        let caps = MicroPythonBackend.capabilities();
        assert!(!caps.contains(Capability::Build));
        assert!(!caps.contains(Capability::Clean));
        assert!(!caps.contains(Capability::BoardSelect));
        assert!(caps.contains(Capability::Repl));
        assert!(caps.contains(Capability::Filesystem));
    }

    #[test]
    fn pyproject_without_markers_contributes_nothing() {
        let scan = DirScan::from_parts(
            "/p",
            ["pyproject.toml"],
            [],
            [(
                "pyproject.toml",
                "[project]\nname = \"x\"\ndependencies = [\"httpx\"]",
            )],
        );
        assert_eq!(weight(&scan), 0.0);
    }

    #[test]
    fn markers_are_matched_case_insensitively() {
        let scan = DirScan::from_parts(
            "/p",
            ["pyproject.toml"],
            [],
            [("pyproject.toml", "[tool.MicroPython]\nboard = \"esp32\"")],
        );
        assert!(
            MicroPythonBackend
                .detect(&scan)
                .iter()
                .any(|s| s.id == "pyproject-micropython")
        );
    }

    #[test]
    fn mpremote_config_is_recognized_by_any_known_name() {
        for name in MPREMOTE_CONFIG_FILES {
            let scan = DirScan::from_parts("/p", [*name], [], []);
            assert!(
                MicroPythonBackend
                    .detect(&scan)
                    .iter()
                    .any(|s| s.id == "mpremote-config"),
                "{name} was not recognized"
            );
        }
    }

    #[test]
    fn signal_weights_are_stable() {
        // Guards the scoring table against accidental edits.
        let scan = DirScan::from_parts("/p", ["boot.py", "main.py"], [], []);
        assert_eq!(weight(&scan), 1.5 + 0.75 + 0.25);
    }
}
