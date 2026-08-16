//! What a brand-new project starts with.
//!
//! `SPEC.md` §7: answering the empty-project prompt is what makes an empty
//! directory usable, so the answer has to leave behind something the backend
//! can actually operate on --- a MicroPython project with the two
//! directories the file browser and the firmware downloader expect, a Zephyr
//! application `west build` accepts without further editing.
//!
//! Each backend declares its own layout ([`crate::backend::Backend::scaffold`]);
//! this module only writes it. That is the same split detection has: the
//! backend knows what its projects look like, the shared code never branches
//! on which backend it is (`AGENTS.md` §3).
//!
//! Writing never overwrites: a file already in the directory is left exactly
//! as it is and reported as skipped, so re-running the prompt on a
//! half-created project completes it instead of resetting it.

use std::io;
use std::path::{Path, PathBuf};

/// One file a new project starts with, at a path relative to the project
/// root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldFile {
    pub path: PathBuf,
    pub contents: String,
}

impl ScaffoldFile {
    pub fn new(path: impl Into<PathBuf>, contents: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            contents: contents.into(),
        }
    }
}

/// A backend's starting layout: directories that must exist even when empty
/// (MicroPython's `firmware/` holds downloads, not sources), plus files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scaffold {
    pub dirs: Vec<PathBuf>,
    pub files: Vec<ScaffoldFile>,
}

impl Scaffold {
    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.files.is_empty()
    }
}

/// What [`create`] did, for the log line that follows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Created {
    /// Paths written, relative to the project root.
    pub written: Vec<PathBuf>,
    /// Files that were already there and were left untouched.
    pub skipped: Vec<PathBuf>,
}

/// Writes `scaffold` into `dir`, creating parent directories as needed.
///
/// A relative path that tries to climb out of the project root
/// (`..`, or an absolute path) is refused --- scaffolds are backend-declared
/// data, and this is the one place that turns them into filesystem writes.
pub fn create(dir: &Path, scaffold: &Scaffold) -> io::Result<Created> {
    let mut created = Created::default();
    for relative in &scaffold.dirs {
        let target = resolve(dir, relative)?;
        std::fs::create_dir_all(&target)?;
    }
    for file in &scaffold.files {
        let target = resolve(dir, &file.path)?;
        if target.exists() {
            created.skipped.push(file.path.clone());
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &file.contents)?;
        created.written.push(file.path.clone());
    }
    Ok(created)
}

fn resolve(dir: &Path, relative: &Path) -> io::Result<PathBuf> {
    let sane = relative.is_relative()
        && !relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir));
    if !sane {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("scaffold path escapes the project: {}", relative.display()),
        ));
    }
    Ok(dir.join(relative))
}

/// A project name reduced to what CMake and shell-free tooling accept:
/// letters, digits, `_` and `-`, with everything else folded to `_`. Empty
/// input (a root directory, a name of only separators) becomes `app`.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "app".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-scaffold-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scaffold() -> Scaffold {
        Scaffold {
            dirs: vec![PathBuf::from("firmware")],
            files: vec![
                ScaffoldFile::new("src/main.py", "print('hi')\n"),
                ScaffoldFile::new("README.md", "docs\n"),
            ],
        }
    }

    #[test]
    fn create_writes_files_and_directories() {
        let dir = temp_dir("write");
        let created = create(&dir, &scaffold()).unwrap();

        let main = std::fs::read_to_string(dir.join("src/main.py")).unwrap();
        let firmware = dir.join("firmware").is_dir();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(main, "print('hi')\n");
        assert!(firmware, "an empty declared directory is still created");
        assert_eq!(created.written.len(), 2);
        assert!(created.skipped.is_empty());
    }

    #[test]
    fn create_never_overwrites_an_existing_file() {
        let dir = temp_dir("keep");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.py"), "mine\n").unwrap();

        let created = create(&dir, &scaffold()).unwrap();

        let main = std::fs::read_to_string(dir.join("src/main.py")).unwrap();
        let readme = dir.join("README.md").exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(main, "mine\n", "an existing file survives untouched");
        assert!(readme, "the missing half is still completed");
        assert_eq!(created.skipped, vec![PathBuf::from("src/main.py")]);
        assert_eq!(created.written, vec![PathBuf::from("README.md")]);
    }

    #[test]
    fn a_path_leaving_the_project_is_refused() {
        let dir = temp_dir("escape");
        let escaping = Scaffold {
            dirs: Vec::new(),
            files: vec![ScaffoldFile::new("../outside.txt", "no\n")],
        };
        let result = create(&dir, &escaping);
        let leaked = dir.parent().unwrap().join("outside.txt").exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert!(!leaked);
    }

    #[test]
    fn safe_name_keeps_what_cmake_accepts() {
        assert_eq!(safe_name("blinky"), "blinky");
        assert_eq!(safe_name("my sensor.app"), "my_sensor_app");
        assert_eq!(safe_name("../.."), "app");
    }
}
