//! Local directory listings and local/device comparison.
//!
//! Separate from [`crate::project::DirScan`] on purpose: that type is a
//! *detection* snapshot (entry names plus an allowlist of file contents, no
//! sizes). Browsing needs sizes and ordering, and detection must not start
//! paying for them.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use crate::backend::micropython::parse::RemoteEntry;

/// One entry in a local directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Lists `path`, directories first and then alphabetically --- the ordering a
/// file manager is expected to have.
///
/// Entries that cannot be stat'ed are still listed, with an unknown size:
/// a broken symlink should be visible, not silently missing.
pub fn read_dir(path: &Path) -> io::Result<Vec<LocalEntry>> {
    let mut entries = Vec::new();

    for entry in std::fs::read_dir(path)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = entry.metadata().ok();

        entries.push(LocalEntry {
            name,
            size: metadata.as_ref().map_or(0, std::fs::Metadata::len),
            is_dir: metadata.as_ref().is_some_and(std::fs::Metadata::is_dir),
        });
    }

    sort_entries(&mut entries);
    Ok(entries)
}

fn sort_entries(entries: &mut [LocalEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Sorts a device listing the same way, so both panes read alike.
pub fn sort_remote(entries: &mut [RemoteEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Dot-files, hidden by default so `.git/` does not drown out the comparison.
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// How one name compares between the local directory and the device directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// Byte-identical, confirmed by sha256.
    Identical,
    /// Same size, contents not checked. *Not* proof of equality --- a file
    /// edited without changing length looks like this.
    SameSize,
    /// Different size, or a sha256 check that disagreed.
    Differs,
    /// Present locally only.
    LocalOnly,
    /// Present on the device only.
    DeviceOnly,
    /// A directory on both sides; contents are compared by entering it.
    Directory,
    /// A directory on one side and a file on the other.
    TypeMismatch,
}

impl SyncStatus {
    /// Single-character marker for the file panes.
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Identical => "=",
            Self::SameSize => "≈",
            Self::Differs => "≠",
            Self::LocalOnly => "→",
            Self::DeviceOnly => "←",
            Self::Directory => "·",
            Self::TypeMismatch => "!",
        }
    }

    pub const fn describe(self) -> &'static str {
        match self {
            Self::Identical => "identical (sha256 verified)",
            Self::SameSize => "same size, contents unverified",
            Self::Differs => "differs",
            Self::LocalOnly => "only in the local folder",
            Self::DeviceOnly => "only on the device",
            Self::Directory => "directory on both sides",
            Self::TypeMismatch => "directory on one side, file on the other",
        }
    }

    /// Whether a content check would add information.
    pub const fn is_verifiable(self) -> bool {
        matches!(self, Self::SameSize | Self::Differs | Self::Identical)
    }
}

/// sha256 verdicts by file name: `true` when both sides hashed the same.
pub type Verdicts = BTreeMap<String, bool>;

/// Compares the two directories currently on screen.
///
/// Like `mc`'s directory compare, this is a comparison of what is displayed:
/// the panes may be at unrelated paths, and the result is still meaningful.
/// Sizes decide by default; a sha256 verdict, when present, overrides them.
pub fn compare(
    local: &[LocalEntry],
    remote: &[RemoteEntry],
    verdicts: &Verdicts,
) -> BTreeMap<String, SyncStatus> {
    let locals: BTreeMap<&str, &LocalEntry> = local
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let remotes: BTreeMap<&str, &RemoteEntry> = remote
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();

    let names: BTreeSet<&str> = locals.keys().chain(remotes.keys()).copied().collect();

    names
        .into_iter()
        .map(|name| {
            let status = match (locals.get(name), remotes.get(name)) {
                (Some(local), Some(remote)) => match (local.is_dir, remote.is_dir) {
                    (true, true) => SyncStatus::Directory,
                    (false, false) => match verdicts.get(name) {
                        Some(true) => SyncStatus::Identical,
                        Some(false) => SyncStatus::Differs,
                        None if local.size == remote.size => SyncStatus::SameSize,
                        None => SyncStatus::Differs,
                    },
                    _ => SyncStatus::TypeMismatch,
                },
                (Some(_), None) => SyncStatus::LocalOnly,
                (None, Some(_)) => SyncStatus::DeviceOnly,
                // Names come from the two maps, so this cannot occur.
                (None, None) => SyncStatus::TypeMismatch,
            };
            (name.to_string(), status)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str, size: u64) -> LocalEntry {
        LocalEntry {
            name: name.into(),
            size,
            is_dir: false,
        }
    }

    fn local_dir(name: &str) -> LocalEntry {
        LocalEntry {
            name: name.into(),
            size: 0,
            is_dir: true,
        }
    }

    fn remote(name: &str, size: u64) -> RemoteEntry {
        RemoteEntry {
            name: name.into(),
            size,
            is_dir: false,
        }
    }

    fn remote_dir(name: &str) -> RemoteEntry {
        RemoteEntry {
            name: name.into(),
            size: 0,
            is_dir: true,
        }
    }

    #[test]
    fn matching_sizes_are_reported_as_unverified() {
        let statuses = compare(
            &[local("main.py", 100)],
            &[remote("main.py", 100)],
            &Verdicts::new(),
        );
        // Deliberately not `Identical`: equal length is not equal content.
        assert_eq!(statuses["main.py"], SyncStatus::SameSize);
    }

    #[test]
    fn differing_sizes_need_no_hash() {
        let statuses = compare(
            &[local("main.py", 100)],
            &[remote("main.py", 90)],
            &Verdicts::new(),
        );
        assert_eq!(statuses["main.py"], SyncStatus::Differs);
    }

    #[test]
    fn a_hash_verdict_overrides_the_size_comparison() {
        let local = [local("main.py", 100)];
        let remote = [remote("main.py", 100)];

        let verdicts = Verdicts::from([("main.py".to_string(), true)]);
        assert_eq!(
            compare(&local, &remote, &verdicts)["main.py"],
            SyncStatus::Identical
        );

        // Same length, different bytes --- exactly the case sizes cannot catch.
        let verdicts = Verdicts::from([("main.py".to_string(), false)]);
        assert_eq!(
            compare(&local, &remote, &verdicts)["main.py"],
            SyncStatus::Differs
        );
    }

    #[test]
    fn one_sided_entries_point_the_right_way() {
        let statuses = compare(
            &[local("only_here.py", 1)],
            &[remote("only_there.py", 1)],
            &Verdicts::new(),
        );
        assert_eq!(statuses["only_here.py"], SyncStatus::LocalOnly);
        assert_eq!(statuses["only_there.py"], SyncStatus::DeviceOnly);
    }

    #[test]
    fn directories_are_compared_by_presence_only() {
        let statuses = compare(&[local_dir("lib")], &[remote_dir("lib")], &Verdicts::new());
        assert_eq!(statuses["lib"], SyncStatus::Directory);
        assert!(!SyncStatus::Directory.is_verifiable());
    }

    #[test]
    fn a_directory_facing_a_file_is_flagged() {
        let statuses = compare(&[local_dir("lib")], &[remote("lib", 40)], &Verdicts::new());
        assert_eq!(statuses["lib"], SyncStatus::TypeMismatch);
    }

    #[test]
    fn empty_sides_still_compare() {
        assert!(compare(&[], &[], &Verdicts::new()).is_empty());

        let statuses = compare(&[local("boot.py", 1)], &[], &Verdicts::new());
        assert_eq!(statuses["boot.py"], SyncStatus::LocalOnly);
    }

    #[test]
    fn every_status_has_a_distinct_marker() {
        let all = [
            SyncStatus::Identical,
            SyncStatus::SameSize,
            SyncStatus::Differs,
            SyncStatus::LocalOnly,
            SyncStatus::DeviceOnly,
            SyncStatus::Directory,
            SyncStatus::TypeMismatch,
        ];
        let markers: BTreeSet<&str> = all.iter().map(|status| status.marker()).collect();
        assert_eq!(markers.len(), all.len());
    }

    #[test]
    fn listings_put_directories_first() {
        let mut entries = vec![
            local("zebra.py", 1),
            local_dir("src"),
            local("apple.py", 1),
            local_dir("Assets"),
        ];
        sort_entries(&mut entries);

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Assets", "src", "apple.py", "zebra.py"]);
    }

    #[test]
    fn remote_listings_sort_identically() {
        let mut entries = vec![remote("main.py", 1), remote_dir("lib")];
        sort_remote(&mut entries);
        assert_eq!(entries[0].name, "lib");
    }

    #[test]
    fn dotfiles_are_hidden() {
        assert!(is_hidden(".git"));
        assert!(is_hidden(".mpremote.toml"));
        assert!(!is_hidden("main.py"));
    }

    #[test]
    fn reads_a_real_directory() {
        let dir = std::env::temp_dir().join(format!("chiptui-files-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("main.py"), "print('hi')\n").unwrap();

        let entries = read_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(entries[0].name, "lib");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "main.py");
        assert_eq!(entries[1].size, 12);
    }
}
