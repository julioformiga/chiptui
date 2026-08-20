//! Version strings, parsed and compared.
//!
//! The installer's checklist is the one place in ChipTUI that asks a tool
//! how old it is. Everywhere else a version is *read from a file*
//! (`zephyr/VERSION`, the venv's `pyvenv.cfg`, an SDK's `sdk_version` ---
//! see [`crate::backend::zephyr::workspace`]) and presence is a filesystem
//! stat ([`crate::backend::executable_at`]). Neither answers "is this cmake
//! new enough", which is a prerequisite question, not a workspace one.
//!
//! Every tool prints its version in its own shape, so the parse is
//! deliberately loose: find the first `N.N[.N]` run of digits in the
//! output and take it. That covers `cmake version 3.28.3`,
//! `Version: DTC 1.7.0`, `pyenv 2.4.7` and `Python 3.12.13` with one rule
//! instead of four, and a tool that changes its wording keeps working as
//! long as it still prints a number.

use std::fmt;

/// A dotted version, compared component by component. A missing patch
/// level reads as `0`, so `3.28` and `3.28.0` are the same version --- the
/// minimums this module checks against are written that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Whether this version satisfies `minimum`.
    pub fn at_least(self, minimum: Self) -> bool {
        self >= minimum
    }

    /// Whether this version's `major.minor` matches `other`'s --- the
    /// question the Python row asks (3.12.13 and 3.12.3 are both "3.12").
    pub fn same_series(self, other: Self) -> bool {
        self.major == other.major && self.minor == other.minor
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The first dotted number in `text`, whatever surrounds it. `None` when
/// the output carries no version at all --- which is an answer the
/// checklist shows ("unreadable"), never a guess.
pub fn parse(text: &str) -> Option<Version> {
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_digit() {
            start += 1;
            continue;
        }
        // A digit run is only a version if a dot and another digit follow:
        // the `2` in "dtc 2 of 3" must not be taken for one.
        if let Some(version) = read_version(text, start) {
            return Some(version);
        }
        start = end_of_run(bytes, start);
    }
    None
}

/// Reads a dotted version starting at `start`, or `None` when the digit run
/// there is not followed by `.` and another digit.
fn read_version(text: &str, start: usize) -> Option<Version> {
    let bytes = text.as_bytes();
    let (major, at) = read_number(bytes, start)?;
    if bytes.get(at) != Some(&b'.') {
        return None;
    }
    let (minor, at) = read_number(bytes, at + 1)?;
    let patch = if bytes.get(at) == Some(&b'.') {
        read_number(bytes, at + 1).map_or(0, |(patch, _)| patch)
    } else {
        0
    };
    Some(Version::new(major, minor, patch))
}

fn read_number(bytes: &[u8], start: usize) -> Option<(u32, usize)> {
    if !bytes.get(start).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let end = end_of_run(bytes, start);
    // A run long enough to overflow is not a version number.
    std::str::from_utf8(&bytes[start..end])
        .ok()?
        .parse()
        .ok()
        .map(|value| (value, end))
}

/// One past the digit run at `start`, always advancing at least one byte
/// so the scan in [`parse`] cannot stall.
fn end_of_run(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    end.max(start + 1)
}

/// The newest `major.minor.patch` release of `series` (`3.12`) offered by
/// `pyenv install --list`. The listing is one candidate per indented line
/// and carries far more than CPython --- `anaconda3-2024.02`,
/// `pypy3.10-7.3.15`, `3.13.0rc1` --- so only bare, fully dotted numeric
/// names qualify: a pre-release or an alternative implementation is never
/// what "the recommended Python" means.
pub fn latest_release(listing: &str, series: Version) -> Option<Version> {
    listing
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && line
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
        })
        .filter_map(|line| {
            let version = parse(line)?;
            // `parse` is loose by design; here the whole name has to be the
            // version, or `3.12.1-foo` would pass as `3.12.1`.
            (version.to_string() == line && version.same_series(series)).then_some(version)
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tools_own_wording_yields_the_same_shape() {
        assert_eq!(parse("cmake version 3.28.3"), Some(Version::new(3, 28, 3)));
        assert_eq!(parse("Version: DTC 1.7.0"), Some(Version::new(1, 7, 0)));
        assert_eq!(parse("pyenv 2.4.7"), Some(Version::new(2, 4, 7)));
        assert_eq!(parse("Python 3.12.13"), Some(Version::new(3, 12, 13)));
        // A two-component version is complete; the patch level reads as 0.
        assert_eq!(parse("cmake version 4.1"), Some(Version::new(4, 1, 0)));
    }

    #[test]
    fn a_bare_number_is_not_a_version() {
        // The digit run before the real version must not swallow it.
        assert_eq!(
            parse("dtc 2 of 3, version 1.6.1"),
            Some(Version::new(1, 6, 1))
        );
        assert_eq!(parse("no version here"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("42"), None);
    }

    #[test]
    fn comparison_orders_by_component_not_by_text() {
        // The whole point of parsing: "3.9.0" > "3.28.0" as strings.
        assert!(Version::new(3, 28, 0).at_least(Version::new(3, 9, 0)));
        assert!(!Version::new(3, 9, 0).at_least(Version::new(3, 28, 0)));
        assert!(Version::new(1, 7, 0).at_least(Version::new(1, 4, 6)));
        assert!(!Version::new(1, 4, 5).at_least(Version::new(1, 4, 6)));
        // The minimum itself passes.
        assert!(Version::new(3, 28, 0).at_least(Version::new(3, 28, 0)));
    }

    #[test]
    fn the_python_row_asks_about_the_series_not_the_patch() {
        assert!(Version::new(3, 12, 13).same_series(Version::new(3, 12, 0)));
        assert!(!Version::new(3, 11, 9).same_series(Version::new(3, 12, 0)));
    }

    #[test]
    fn the_latest_release_skips_prereleases_and_other_implementations() {
        let listing = "\
Available versions:
  3.11.9
  3.12.1
  3.12.13
  3.12.0rc1
  3.13.0
  pypy3.12-7.3.16
  anaconda3-2024.02
";
        assert_eq!(
            latest_release(listing, Version::new(3, 12, 0)),
            Some(Version::new(3, 12, 13))
        );
        // A series pyenv does not offer has no answer --- the step says so
        // rather than falling back to a neighbouring one.
        assert_eq!(latest_release(listing, Version::new(3, 14, 0)), None);
    }
}
