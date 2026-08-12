//! Directory snapshots.
//!
//! Detection heuristics run against a [`DirScan`] instead of the filesystem.
//! That keeps every scoring rule testable in memory and makes the number of
//! syscalls per candidate directory predictable (one `read_dir` plus at most
//! [`TEXT_FILES`] small reads).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

/// Files whose *contents* carry detection evidence.
///
/// Reading is eager and restricted to this list: a backend must not be able to
/// trigger arbitrary I/O from inside a scoring function.
pub const TEXT_FILES: &[&str] = &[
    "pyproject.toml",
    "CMakeLists.txt",
    "requirements.txt",
    "west.yml",
    "chiptui.toml",
];

/// Contents above this size are ignored; detection markers all live near the
/// top of small config files, and this bounds the work per directory.
const MAX_TEXT_BYTES: u64 = 128 * 1024;

/// An immutable view of one directory.
#[derive(Debug, Clone, Default)]
pub struct DirScan {
    path: PathBuf,
    files: BTreeSet<String>,
    dirs: BTreeSet<String>,
    texts: BTreeMap<String, String>,
}

impl DirScan {
    /// Reads `path`, collecting entry names and the contents of [`TEXT_FILES`].
    ///
    /// Unreadable individual entries are skipped rather than failing the whole
    /// scan: a project directory with one permission-denied file is still worth
    /// detecting.
    pub fn read(path: &Path) -> io::Result<Self> {
        let mut scan = Self {
            path: path.to_path_buf(),
            ..Self::default()
        };

        for entry in std::fs::read_dir(path)? {
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    scan.dirs.insert(name);
                }
                Ok(_) => {
                    if TEXT_FILES.contains(&name.as_str())
                        && let Some(text) = read_small_file(&entry.path())
                    {
                        scan.texts.insert(name.clone(), text);
                    }
                    scan.files.insert(name);
                }
                Err(_) => continue,
            }
        }

        Ok(scan)
    }

    /// Builds a snapshot without touching the filesystem.
    ///
    /// Primarily for tests; also the seam a future remote/virtual project
    /// source would plug into.
    pub fn from_parts<S: Into<String>>(
        path: impl Into<PathBuf>,
        files: impl IntoIterator<Item = S>,
        dirs: impl IntoIterator<Item = S>,
        texts: impl IntoIterator<Item = (S, S)>,
    ) -> Self {
        Self {
            path: path.into(),
            files: files.into_iter().map(Into::into).collect(),
            dirs: dirs.into_iter().map(Into::into).collect(),
            texts: texts
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// A snapshot of a directory with no entries at all.
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn has_file(&self, name: &str) -> bool {
        self.files.contains(name)
    }

    pub fn has_dir(&self, name: &str) -> bool {
        self.dirs.contains(name)
    }

    pub fn has_any_file(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.has_file(name))
    }

    /// Whether any regular file ends with `suffix` (e.g. `".py"`, `".overlay"`).
    pub fn has_file_with_suffix(&self, suffix: &str) -> bool {
        self.files.iter().any(|name| name.ends_with(suffix))
    }

    /// Contents of `name`, if it is in [`TEXT_FILES`] and was readable.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.texts.get(name).map(String::as_str)
    }

    /// Case-insensitive substring search in `name`'s contents.
    ///
    /// `needle` must be lowercase.
    pub fn text_contains(&self, name: &str, needle: &str) -> bool {
        debug_assert_eq!(
            needle,
            needle.to_ascii_lowercase(),
            "needle must be lowercase"
        );
        self.text(name)
            .is_some_and(|text| text.to_ascii_lowercase().contains(needle))
    }
}

fn read_small_file(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_TEXT_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_exposes_files_dirs_and_texts() {
        let scan = DirScan::from_parts(
            "/tmp/demo",
            ["main.py", "pyproject.toml"],
            ["lib"],
            [("pyproject.toml", "[project]\nname = \"demo\"")],
        );

        assert!(scan.has_file("main.py"));
        assert!(!scan.has_file("lib"), "directories are not files");
        assert!(scan.has_dir("lib"));
        assert!(!scan.has_dir("main.py"));
        assert!(scan.has_any_file(&["boot.py", "main.py"]));
        assert!(scan.has_file_with_suffix(".py"));
        assert!(!scan.has_file_with_suffix(".overlay"));
        assert_eq!(scan.path(), Path::new("/tmp/demo"));
    }

    #[test]
    fn text_contains_is_case_insensitive_and_absent_safe() {
        let scan = DirScan::from_parts(
            "/tmp/demo",
            ["CMakeLists.txt"],
            [],
            [(
                "CMakeLists.txt",
                "find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})",
            )],
        );

        assert!(scan.text_contains("CMakeLists.txt", "find_package(zephyr"));
        assert!(!scan.text_contains("CMakeLists.txt", "find_package(qt"));
        // A file with no captured contents never matches.
        assert!(!scan.text_contains("prj.conf", "anything"));
    }
}
