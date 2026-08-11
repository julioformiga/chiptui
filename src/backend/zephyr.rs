//! Zephyr backend.
//!
//! Detection only, for now. Operations will delegate to `west` (and the CMake
//! build system it drives); nothing here shells out yet.

use crate::backend::{Backend, BackendKind, Capabilities, Capability};
use crate::project::{DirScan, Signal};

/// Test/sample metadata files used across the Zephyr tree.
const ZEPHYR_METADATA: &[&str] = &["sample.yaml", "testcase.yaml"];

pub struct ZephyrBackend;

impl Backend for ZephyrBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Zephyr
    }

    fn detect(&self, scan: &DirScan) -> Vec<Signal> {
        let mut signals = Vec::new();

        // Decisive: this line is what makes a CMake project a Zephyr
        // application, and it is what separates the two (SPEC.md §7).
        if scan.text_contains("CMakeLists.txt", "find_package(zephyr") {
            signals.push(Signal::new(
                "cmake-zephyr",
                3.0,
                "CMakeLists.txt calls find_package(Zephyr)",
            ));
        } else if scan.has_file("CMakeLists.txt") {
            signals.push(Signal::new(
                "cmake",
                0.25,
                "CMakeLists.txt present (generic CMake)",
            ));
        }

        if scan.has_file("prj.conf") {
            signals.push(Signal::new("prj.conf", 1.5, "Kconfig fragment prj.conf"));
        }
        if scan.has_dir(".west") {
            signals.push(Signal::new(".west", 1.5, ".west/ workspace directory"));
        }
        if scan.has_any_file(&["west.yml", "west.yaml"]) {
            signals.push(Signal::new("west.yml", 1.0, "west manifest"));
        }
        if scan.has_file_with_suffix(".overlay") {
            signals.push(Signal::new("overlay", 0.5, "devicetree overlay"));
        }
        if scan.has_dir("boards") {
            signals.push(Signal::new("boards", 0.5, "boards/ directory"));
        }
        if scan.has_file("Kconfig") {
            signals.push(Signal::new("Kconfig", 0.25, "Kconfig file"));
        }
        if scan.has_any_file(ZEPHYR_METADATA) {
            signals.push(Signal::new(
                "zephyr-metadata",
                0.5,
                "sample/testcase metadata",
            ));
        }

        signals
    }

    fn saturation(&self) -> f32 {
        4.0
    }

    fn capabilities(&self) -> Capabilities {
        // SPEC.md §6: no filesystem/REPL --- the target runs a compiled image.
        Capabilities::from_slice(&[
            Capability::Build,
            Capability::Clean,
            Capability::Flash,
            Capability::Monitor,
            Capability::BoardSelect,
        ])
    }

    fn required_tools(&self) -> &'static [&'static str] {
        &["west", "cmake", "ninja"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_no_filesystem_or_repl_capability() {
        let caps = ZephyrBackend.capabilities();
        assert!(caps.contains(Capability::Build));
        assert!(caps.contains(Capability::Clean));
        assert!(!caps.contains(Capability::Filesystem));
        assert!(!caps.contains(Capability::Repl));
        assert!(!caps.contains(Capability::Upload));
    }

    #[test]
    fn generic_cmake_does_not_stack_with_the_zephyr_marker() {
        let scan = DirScan::from_parts(
            "/p",
            ["CMakeLists.txt"],
            [],
            [("CMakeLists.txt", "find_package(Zephyr REQUIRED)")],
        );
        let ids: Vec<_> = ZephyrBackend.detect(&scan).iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec!["cmake-zephyr"],
            "generic and Zephyr CMake signals must be exclusive"
        );
    }

    #[test]
    fn generic_cmake_alone_scores_almost_nothing() {
        let scan = DirScan::from_parts(
            "/p",
            ["CMakeLists.txt"],
            [],
            [("CMakeLists.txt", "add_executable(app main.c)")],
        );
        let total: f32 = ZephyrBackend.detect(&scan).iter().map(|s| s.weight).sum();
        assert_eq!(total, 0.25);
    }

    #[test]
    fn both_west_manifest_spellings_are_accepted() {
        for name in ["west.yml", "west.yaml"] {
            let scan = DirScan::from_parts("/p", [name], [], []);
            assert!(
                ZephyrBackend
                    .detect(&scan)
                    .iter()
                    .any(|s| s.id == "west.yml")
            );
        }
    }

    #[test]
    fn any_overlay_file_counts() {
        let scan = DirScan::from_parts("/p", ["nrf52840dk_nrf52840.overlay"], [], []);
        assert!(
            ZephyrBackend
                .detect(&scan)
                .iter()
                .any(|s| s.id == "overlay")
        );
    }
}
