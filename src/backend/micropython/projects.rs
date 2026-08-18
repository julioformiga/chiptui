//! Locating the user's MicroPython *applications*.
//!
//! The Zephyr twin of this module ([`crate::backend::zephyr::projects`])
//! asks "what am I building" and answers with the one file `west build`
//! cannot run without. MicroPython runs source directly (`SPEC.md` §6: no
//! build step), so its bar is the tool's bar: **any** immediate
//! subdirectory of the configured projects folder (`[micropython]
//! projects`, user config only) is a project --- the picker lists them all,
//! marks none, refuses none. What a project *is* stays detection's
//! weighted question ([`super`]'s own job); this is only a filesystem
//! picker's inventory.

use std::path::{Path, PathBuf};

/// One row of the MicroPython project picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub name: String,
    pub path: PathBuf,
}

/// The picker's rows for `dir`: every immediate subdirectory sorted by
/// name. A directory that cannot be read reports why, so the picker never
/// dead-ends.
pub fn project_rows(dir: &Path) -> (Vec<ProjectRow>, Option<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => return (Vec::new(), Some(format!("{}: {err}", dir.display()))),
    };
    let mut rows: Vec<ProjectRow> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| ProjectRow {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path(),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.dedup_by(|a, b| a.name == b.name);
    (rows, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_subdirectory_counts_as_a_project() {
        let dir = std::env::temp_dir().join(format!("chiptui-mpyproj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("blink")).unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("loose.py"), b"").unwrap();

        let (rows, error) = project_rows(&dir);
        assert_eq!(error, None);
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["blink", "empty"], "files never list");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_directory_is_reported() {
        let (rows, error) = project_rows(Path::new("/nonexistent-chiptui-mpy"));
        assert!(rows.is_empty());
        assert!(error.is_some());
    }
}
