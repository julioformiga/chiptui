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

/// Whether re-reading `path` would produce a different listing than
/// `current`: names, sizes and kinds compared, which is exactly the state a
/// Files pane draws. Used by the tick's auto-refresh so an unchanged
/// directory costs one `readdir` and no redraw churn, and a directory that
/// *changed outside the program* (another editor, a build step, a sync tool)
/// is picked up on the next poll.
///
/// An unreadable directory reports `false`, not a change: a transient read
/// failure (a parent mid-rename, a permissions blip) must not blank the pane
/// --- the last good listing stays until a read succeeds again.
pub fn listing_changed(current: &[LocalEntry], path: &Path) -> bool {
    match read_dir(path) {
        Ok(entries) => entries != current,
        Err(_) => false,
    }
}

/// Sorts a device listing the same way, so both panes read alike.
pub fn sort_remote(entries: &mut [RemoteEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Recursively walks `root`, returning every file and directory under it
/// keyed by path relative to `root` (using `/` regardless of host OS so the
/// result maps directly onto device paths). Hidden entries are skipped,
/// mirroring the file browser's default.
///
/// Run synchronously like the rest of the local pane's I/O — acceptable for
/// embedded-project trees (source files, not `node_modules`).
pub fn walk_local(root: &Path) -> (BTreeMap<String, u64>, BTreeSet<String>) {
    let mut files = BTreeMap::new();
    let mut dirs = BTreeSet::new();
    walk_local_into(root, root, &mut files, &mut dirs);
    (files, dirs)
}

fn walk_local_into(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, u64>,
    dirs: &mut BTreeSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_hidden(&name) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(&entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if metadata.is_dir() {
            dirs.insert(relative);
            walk_local_into(root, &entry.path(), files, dirs);
        } else {
            files.insert(relative, metadata.len());
        }
    }
}

/// Dot-files, hidden by default so `.git/` does not drown out the comparison.
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// Files above this are not opened in the viewer (local: read outright,
/// device: from the cached listing's size), to keep the read and the
/// on-screen buffer off the UI thread's back --- `$EDITOR` (`Edit`, in the
/// files-pane action menu) can still open a rejected local file directly,
/// since it does its own paging; a device file that size still downloads
/// fine, `mpremote` streams it rather than buffering it as one `String`.
pub const MAX_VIEW_BYTES: u64 = 2 * 1024 * 1024;

/// Bytes sniffed from the front of a file to decide whether it looks like
/// text, mirroring what `file`/`grep -I` treat as the binary signal.
const BINARY_PROBE_BYTES: usize = 8192;

/// Extensions treated as binary --- excluded from the files-pane action menu
/// entirely, so `enter` on one stays a no-op exactly as it always was. A
/// denylist, not an allowlist: an unfamiliar text extension (a `Makefile`
/// with no extension, a stray `.env`) should still work rather than silently
/// losing its menu.
///
/// The device pane has no content to sniff before a file is fetched, so this
/// is the only signal available there; local files get a second,
/// content-based check in [`read_text_file`].
const BINARY_EXTENSIONS: &[&str] = &[
    // compiled / firmware
    "bin", "elf", "hex", "dfu", "uf2", "o", "a", "so", "dll", "exe", "mpy", "pyc", "class",
    // archives
    "zip", "tar", "gz", "tgz", "xz", "bz2", "7z", "rar", // images
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "svg", // fonts
    "ttf", "otf", "woff", "woff2", "eot", // audio / video
    "mp3", "wav", "ogg", "flac", "mp4", "avi", "mov", "mkv", // documents
    "pdf",
];

/// Whether `name` looks like a text file, judging only by its extension.
pub fn is_text_like(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, ext)) => !BINARY_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()),
        None => true,
    }
}

/// Reads `path` as text for the file-browser viewer.
///
/// Rejects directories, oversized files and anything that looks binary (a
/// NUL byte in the first [`BINARY_PROBE_BYTES`]) instead of dumping raw bytes
/// into the terminal. Decoding is lossy: a file with a few invalid UTF-8
/// bytes should still be readable, with replacement characters standing in,
/// rather than refusing to show it at all.
pub fn read_text_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|source| format!("cannot read: {source}"))?;
    if metadata.is_dir() {
        return Err("is a directory".to_string());
    }
    if metadata.len() > MAX_VIEW_BYTES {
        return Err(format!(
            "too large to preview ({} MiB) --- press 'e' to open it in $EDITOR instead",
            metadata.len() / (1024 * 1024)
        ));
    }

    let bytes = std::fs::read(path).map_err(|source| format!("cannot read: {source}"))?;
    let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
    if probe.contains(&0) {
        return Err("binary file --- press 'e' to open it in $EDITOR instead".to_string());
    }

    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `.bin`/`.elf` firmware candidates in `dir`, non-recursive.
///
/// Built on [`read_dir`]: same hidden-file convention, restricted to files
/// (a directory named `firmware.bin` is not firmware) with a recognised
/// extension. Used to offer an esptool flash after an erase, or on its own,
/// scoped to the project's `firmware/` directory (`SPEC.md` §9).
pub fn firmware_candidates(dir: &Path) -> io::Result<Vec<LocalEntry>> {
    let entries = read_dir(dir)?
        .into_iter()
        .filter(|entry| !entry.is_dir && !is_hidden(&entry.name) && is_firmware_name(&entry.name))
        .collect();
    Ok(entries)
}

fn is_firmware_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".bin") || lower.ends_with(".elf")
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
    fn listing_changed_compares_the_directory_against_the_snapshot() {
        let dir = std::env::temp_dir().join(format!("chiptui-listing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();

        let current = read_dir(&dir).unwrap();
        assert!(!listing_changed(&current, &dir));

        std::fs::write(dir.join("b.txt"), b"y").unwrap();
        assert!(
            listing_changed(&current, &dir),
            "an external create is a change"
        );

        let reloaded = read_dir(&dir).unwrap();
        assert!(!listing_changed(&reloaded, &dir));

        std::fs::remove_file(dir.join("b.txt")).unwrap();
        assert!(
            listing_changed(&reloaded, &dir),
            "an external delete is a change"
        );

        let before = read_dir(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"longer contents").unwrap();
        assert!(
            listing_changed(&before, &dir),
            "a same-name size change is a change"
        );
        // A same-size content rewrite is not: the listing draws name, size
        // and kind, and none of those rows moved.
        let current = read_dir(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"other contents!").unwrap();
        assert!(!listing_changed(&current, &dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_directory_is_not_a_listing_change() {
        let current = vec![local("a.txt", 1)];
        assert!(!listing_changed(
            &current,
            Path::new("/nonexistent-chiptui-path")
        ));
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

    #[test]
    fn firmware_candidates_are_filtered_by_extension() {
        let dir = std::env::temp_dir().join(format!("chiptui-firmware-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("build.bin")).unwrap(); // a directory, not firmware
        std::fs::write(dir.join("app.BIN"), "x").unwrap(); // extension case is ignored
        std::fs::write(dir.join("app.elf"), "x").unwrap();
        std::fs::write(dir.join("readme.txt"), "x").unwrap();
        std::fs::write(dir.join(".hidden.bin"), "x").unwrap();

        let entries = firmware_candidates(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let names: BTreeSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, BTreeSet::from(["app.BIN", "app.elf"]));
    }

    #[test]
    fn text_like_extensions_pass() {
        assert!(is_text_like("main.py"));
        assert!(is_text_like("CMakeLists.txt"));
        assert!(is_text_like("Makefile"), "no extension is not binary");
        assert!(is_text_like(".env"), "a dotfile with no real extension");
    }

    #[test]
    fn binary_extensions_are_excluded_case_insensitively() {
        assert!(!is_text_like("firmware.bin"));
        assert!(!is_text_like("firmware.BIN"));
        assert!(!is_text_like("app.elf"));
        assert!(!is_text_like("bytecode.mpy"));
        assert!(!is_text_like("logo.png"));
        assert!(!is_text_like("font.ttf"));
        assert!(
            !is_text_like("archive.tar.gz"),
            "the last extension decides"
        );
    }

    #[test]
    fn reads_a_text_file_for_the_viewer() {
        let dir = std::env::temp_dir().join(format!("chiptui-view-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.py"), "print('hi')\n").unwrap();

        let content = read_text_file(&dir.join("main.py")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(content, "print('hi')\n");
    }

    #[test]
    fn viewer_rejects_directories() {
        let dir = std::env::temp_dir().join(format!("chiptui-view-dir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();

        let error = read_text_file(&dir.join("lib")).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(error.contains("directory"));
    }

    #[test]
    fn viewer_rejects_binary_content() {
        let dir = std::env::temp_dir().join(format!("chiptui-view-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("firmware.bin"),
            [0x7f, 0x45, 0x4c, 0x46, 0x00, 0x01],
        )
        .unwrap();

        let error = read_text_file(&dir.join("firmware.bin")).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(error.contains("binary"));
    }

    #[test]
    fn viewer_rejects_oversized_files() {
        let dir = std::env::temp_dir().join(format!("chiptui-view-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = vec![b'a'; (MAX_VIEW_BYTES + 1) as usize];
        std::fs::write(dir.join("huge.txt"), &big).unwrap();

        let error = read_text_file(&dir.join("huge.txt")).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(error.contains("large"));
    }

    #[test]
    fn firmware_candidates_is_empty_when_none_match() {
        let dir = std::env::temp_dir().join(format!("chiptui-no-firmware-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.py"), "x").unwrap();

        let entries = firmware_candidates(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(entries.is_empty());
    }

    #[test]
    fn walk_local_finds_files_and_directories_recursively() {
        let dir = std::env::temp_dir().join(format!("chiptui-walk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("lib/nested")).unwrap();
        std::fs::write(dir.join("main.py"), "print('hi')\n").unwrap(); // 12 bytes
        std::fs::write(dir.join("lib/util.py"), "x = 1\n").unwrap(); // 6 bytes
        std::fs::write(dir.join("lib/nested/deep.py"), "y = 2\n").unwrap(); // 6 bytes
        std::fs::write(dir.join(".hidden"), "secret").unwrap();

        let (files, dirs) = walk_local(&dir);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(files.len(), 3);
        assert_eq!(files["main.py"], 12);
        assert_eq!(files["lib/util.py"], 6);
        assert_eq!(files["lib/nested/deep.py"], 6);
        assert!(dirs.contains("lib"));
        assert!(dirs.contains("lib/nested"));
    }

    #[test]
    fn walk_local_on_a_missing_directory_is_empty() {
        let (files, dirs) = walk_local(std::path::Path::new("/nonexistent-chiptui-walk"));
        assert!(files.is_empty());
        assert!(dirs.is_empty());
    }
}
