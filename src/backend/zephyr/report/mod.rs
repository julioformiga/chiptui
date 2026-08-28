//! The build directory's own report artifacts, read natively.
//!
//! Zephyr 4.4 ships `west build -t dashboard`
//! (`<zephyr>/scripts/dashboard/dashboard.py`), which renders a build
//! directory into a multi-page HTML report and opens it in a browser. This
//! module answers the same questions inside the terminal instead, and it does
//! so without the HTML: almost everything that page shows already exists as
//! plain text or JSON in the build directory, written by the build itself.
//!
//! | artifact | page it answers |
//! |---|---|
//! | `build_info.yml` | Build Summary |
//! | `zephyr/zephyr.stat` | Build Summary's memory figures, and ELF Stats whole |
//! | `zephyr/.config-trace.json` | Kconfig |
//! | `zephyr/zephyr.dts` | Device Tree |
//! | `<build>/dashboard/{all,ram,rom}_report.json` | Memory Report |
//!
//! Only the last one has to be *produced*: it comes from
//! `scripts/footprint/size_report`, which needs the ELF's DWARF to map
//! symbols back to source files. That is the one place this feature delegates
//! to Python, and it delegates to Zephyr's own script with its own `--json`
//! output --- the `CLAUDE.md` rule (#2 delegate to external CLIs, #3 prefer
//! machine-readable tool output) rather than a reimplementation. The other
//! four artifacts need no tool at all, so four of the five tabs work on a
//! machine with none of `dashboard.py`'s dependencies (`jinja2`, `pygments`,
//! `plotly`) installed.
//!
//! Every parser here is a pure function from `&str` to a value: no
//! filesystem, no process, no UI. That is what makes them testable against
//! fixtures cut from a real build, which is where the shape of each format
//! was confirmed --- and every fixture in this subtree is a cut of a real
//! artifact, not an invention, because that is the only way the shapes that
//! matter show up (a `.text` section flagged `WAX` rather than `AX`; 64
//! Kconfig assignments with no location at all; a devicetree property
//! spanning five lines).
//!
//! # Cost
//!
//! All of these are read on the UI thread when a tab is entered, never in
//! the draw path (`CLAUDE.md`'s rule, which `requirements.txt` learned the
//! hard way). Measured on a real project --- an ESP32-C3 with LVGL, a
//! debug build of this crate, so a conservative bound:
//!
//! | artifact | size | parse |
//! |---|---|---|
//! | `build_info.yml` | 2.8 KB | under a millisecond |
//! | `zephyr.stat` | 8.7 KB | under a millisecond |
//! | `zephyr.dts` | 52 KB | 2 ms |
//! | `.config-trace.json` | 415 KB / 2190 symbols | 56 ms |
//! | `all_report.json` | 4.5 MB / 6309 nodes | 185 ms |
//!
//! A release build is roughly an order of magnitude faster again, so the
//! largest of them costs a fraction of a frame on a gesture the user just
//! made. None of this needs the background-thread machinery
//! [`crate::board_docs`] has --- that exists for *network* fetches which
//! can take seconds or never return.

use std::path::{Path, PathBuf};

pub mod build_info;
pub mod devicetree;
pub mod elf_stat;
pub mod json;
pub mod kconfig;
pub mod memory;

/// Where each artifact lives, given a project root and a build directory.
///
/// One definition rather than a `join` at every call site: the two
/// dashboards must look in the same places, and `<build>/dashboard/` in
/// particular is not obvious --- it is where Zephyr's own `dashboard` target
/// writes its reports, and reusing it is what lets the TUI and the HTML
/// report share a `size_report` run that costs a minute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportPaths {
    /// `<root>/<build_dir>`.
    pub build: PathBuf,
    /// `<build>/dashboard` --- shared with `west build -t dashboard`.
    pub output: PathBuf,
}

impl ReportPaths {
    pub fn new(root: &Path, build_dir: &str) -> Self {
        let build = root.join(build_dir);
        let output = build.join("dashboard");
        Self { build, output }
    }

    pub fn build_info(&self) -> PathBuf {
        self.build.join("build_info.yml")
    }

    pub fn elf(&self) -> PathBuf {
        self.build.join("zephyr").join("zephyr.elf")
    }

    pub fn bin(&self) -> PathBuf {
        self.build.join("zephyr").join("zephyr.bin")
    }

    pub fn stat(&self) -> PathBuf {
        self.build.join("zephyr").join("zephyr.stat")
    }

    pub fn config_trace(&self) -> PathBuf {
        self.build.join("zephyr").join(".config-trace.json")
    }

    pub fn config(&self) -> PathBuf {
        self.build.join("zephyr").join(".config")
    }

    pub fn devicetree(&self) -> PathBuf {
        self.build.join("zephyr").join("zephyr.dts")
    }

    /// One of `all`, `ram` or `rom`.
    pub fn memory_report(&self, target: &str) -> PathBuf {
        self.output.join(format!("{target}_report.json"))
    }

    /// The C compiler description CMake writes, whose directory is named
    /// after the CMake version --- so it is found rather than composed.
    pub fn cmake_compiler(&self) -> Option<PathBuf> {
        let entries = std::fs::read_dir(self.build.join("CMakeFiles")).ok()?;
        entries.flatten().find_map(|entry| {
            let candidate = entry.path().join("CMakeCCompiler.cmake");
            candidate.is_file().then_some(candidate)
        })
    }
}

/// A file's modification time and length --- the change test.
///
/// The same pair, for the same reason, as [`crate::files::listing_changed`]:
/// an unreadable file answers `None` and is *never* a change, so a transient
/// failure re-serves the good data already parsed instead of blanking a tab.
pub type Stamp = (std::time::SystemTime, u64);

pub fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Whether the memory reports are older than the ELF they describe.
///
/// This is `dashboard.py`'s own staleness test
/// (`getmtime(all_report.json) < getmtime(zephyr.elf)`), reproduced exactly
/// so the two dashboards never disagree about one build --- a report
/// generated in the same second as the ELF counts as current for both.
pub fn memory_report_stale(paths: &ReportPaths) -> bool {
    let Some((report, _)) = stamp(&paths.memory_report("all")) else {
        return true;
    };
    match stamp(&paths.elf()) {
        Some((elf, _)) => report < elf,
        // No ELF to be older than: whatever the report holds is all there is.
        None => false,
    }
}

/// A byte count in the form the HTML dashboard uses.
///
/// This reproduces `dashboard.py::display_size`: the largest unit whose size
/// the count reaches, one decimal place, and a trailing `.0` dropped so a
/// round number reads as `10 KB` rather than `10.0 KB`. The two dashboards
/// describe the same build, so they should describe it in the same words.
///
/// (The Python does the last step with a *global* `str.replace('.0', '')`,
/// which looks like it would also eat a `.0` inside the integer part. It
/// cannot: the integer part of a `{:.1f}` never contains a dot, so the only
/// `.0` in the string is the fractional one. Checked against the real
/// function before copying it, rather than assumed either way.)
pub fn display_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1024 * 1024 * 1024, "GB"),
        (1024 * 1024, "MB"),
        (1024, "KB"),
        (1, "Bytes"),
    ];
    for (size, unit) in UNITS {
        if bytes >= size {
            let scaled = bytes as f64 / size as f64;
            let text = format!("{scaled:.1}");
            let text = text.strip_suffix(".0").unwrap_or(&text);
            return format!("{text} {unit}");
        }
    }
    // Only zero reaches here --- `1` is the last unit's threshold.
    "0 Bytes".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case checked against `dashboard.py::display_size` itself, run
    /// over the same inputs.
    #[test]
    fn sizes_read_the_way_the_html_dashboard_writes_them() {
        assert_eq!(display_size(0), "0 Bytes");
        assert_eq!(display_size(512), "512 Bytes");
        assert_eq!(display_size(1023), "1023 Bytes");
        assert_eq!(display_size(1024), "1 KB");
        assert_eq!(display_size(10240), "10 KB");
        assert_eq!(display_size(102400), "100 KB");
        assert_eq!(display_size(1126400), "1.1 MB");
        assert_eq!(display_size(10485760), "10 MB");
        assert_eq!(display_size(1024 * 1024 * 1024), "1 GB");
    }

    /// The unit boundary is `>=`, so exactly one kilobyte is `1 KB` and one
    /// byte less is still bytes --- the Python's own `abs(x) >= unit_size`.
    #[test]
    fn the_unit_switches_exactly_at_the_boundary() {
        assert_eq!(display_size(1024 * 1024 - 1), "1024 KB");
        assert_eq!(display_size(1024 * 1024), "1 MB");
    }
}
