//! Locating the user's Zephyr *applications* and telling one from a plain
//! directory.
//!
//! The installation ([`super::workspace`]) answers "where is Zephyr"; this
//! module answers "what am I building". A user's applications live in some
//! folder of their choosing --- anywhere on disk, unrelated to the
//! installation --- and one of them is the build panel's working directory:
//!
//! 1. the **projects directory** (`[zephyr] projects`), resolved from the
//!    same two config levels as the installation and never guessed;
//! 2. the **project** itself: a subdirectory that contains build elements.
//!    `west build` needs a `CMakeLists.txt` declaring a Zephyr application,
//!    and that one file is the difference between an application and a
//!    folder, so it is the whole test ([`is_buildable`]) --- no weighted
//!    scan here, this is a filesystem picker, not project *detection*
//!    ([`super::detect`] keeps that job and its explainability). The
//!    listing looks one level deeper through a directory that is not itself
//!    an application, which is what finds the `app/` of a repository whose
//!    root is an out-of-tree board module.
//!
//! Both halves feed the same gate: before any project command (build, clean,
//! flash, menuconfig) runs, its working directory must pass
//! [`is_buildable`]. A directory that fails is never built silently --- the
//! picker marks it, and accepting one keeps the picker open with the reason.

use std::path::{Path, PathBuf};

use super::workspace::ResolveInput;
use crate::settings::expand_home;

/// The one file `west build` cannot run without: a Zephyr application's
/// build entry point. [`is_buildable`] reads it, rather than merely
/// stat-ing it --- see there.
const BUILD_ENTRY: &str = "CMakeLists.txt";

/// The outcome of resolving the projects directory from configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectsResolution {
    /// A configured location that exists.
    Configured(PathBuf),
    /// A configured location that is not there; the message is the
    /// actionable explanation (`SPEC.md` §14).
    Invalid(String),
    /// No config names a folder: the workspace pane's chooser answers this.
    NotConfigured,
}

/// Resolves the projects directory: the project's `chiptui.toml` first (a
/// project pinned to a specific apps folder must not fight the machine's
/// default), then the user config. Neither names one → ask, never guess.
pub fn resolve(input: &ResolveInput<'_>) -> ProjectsResolution {
    if let Some(dir) = input
        .project_settings
        .filter(|s| s.projects.is_some())
        .map(|s| expand_home(s.projects.as_deref().unwrap_or_default(), input.home))
    {
        return dir_check(dir);
    }
    if let Some(dir) = input
        .user_settings
        .filter(|s| s.projects.is_some())
        .map(|s| expand_home(s.projects.as_deref().unwrap_or_default(), input.home))
    {
        return dir_check(dir);
    }
    ProjectsResolution::NotConfigured
}

/// Validates a candidate projects directory: it only has to *exist* (its
/// subdirectories carry the build-element test). Public because the
/// directory picker validates a user-chosen folder through the exact same
/// rule the config goes through --- one definition, two doors.
pub fn dir_check(dir: PathBuf) -> ProjectsResolution {
    if dir.is_dir() {
        ProjectsResolution::Configured(dir)
    } else {
        ProjectsResolution::Invalid(format!(
            "{} does not exist (or is not a directory) — fix [zephyr] projects or choose again",
            dir.display()
        ))
    }
}

/// Whether `dir` holds the elements a build needs: a `CMakeLists.txt` that
/// declares a Zephyr application. The bar is deliberately the *tool's* bar,
/// not detection's weighted evidence --- this answers "can the command run
/// here", not "what kind of project is this".
///
/// The file's *presence* is not the bar, because a repository that
/// contributes an out-of-tree board is a Zephyr **module**, and a module
/// carries a `CMakeLists.txt` too --- an empty one, whose whole content is
/// a comment saying the module adds no sources. Accepting it let the gate
/// pass on a directory where `west build` then failed with its own
/// "source directory does not contain a CMakeLists.txt with a Zephyr
/// application" while the real application sat one level down in `app/`.
/// `find_package(Zephyr` is what an application has and a module hook does
/// not --- the same evidence detection already weighs heaviest.
pub fn is_buildable(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(BUILD_ENTRY))
        .is_ok_and(|text| text.to_lowercase().contains("find_package(zephyr"))
}

/// One row of the project picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub name: String,
    pub path: PathBuf,
    /// Whether [`is_buildable`] passed --- the ✓ (or the reason it cannot)
    /// the picker shows next to the name.
    pub buildable: bool,
}

/// The project picker's rows for `dir`: every immediate subdirectory sorted
/// by name, each marked buildable or not. Non-buildable directories are
/// listed (dimmed, with their missing element) rather than hidden --- an
/// honest inventory beats a mystery omission --- but cannot be accepted. A
/// directory that cannot be read reports why, so the picker never dead-ends.
///
/// A subdirectory that is not itself an application is searched **one level
/// deeper**, and its buildable children join the list as `parent/child`.
/// That is not a general recursive walk: it is the one extra level the
/// out-of-tree board layout costs, where the repository is the module and
/// the application lives beside the `boards/` tree it uses. Without it the
/// only row such a repository offers is its own unbuildable root.
pub fn project_rows(dir: &Path) -> (Vec<ProjectRow>, Option<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => return (Vec::new(), Some(format!("{}: {err}", dir.display()))),
    };
    let mut rows: Vec<ProjectRow> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_buildable(&path) {
            rows.push(ProjectRow {
                name,
                path,
                buildable: true,
            });
            continue;
        }
        let nested = nested_applications(&path, &name);
        if nested.is_empty() {
            rows.push(ProjectRow {
                name,
                path,
                buildable: false,
            });
        } else {
            rows.extend(nested);
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.dedup_by(|a, b| a.name == b.name);
    (rows, None)
}

/// The buildable immediate children of `dir`, named `parent/child`. Empty
/// for anything that has none --- including an unreadable directory, which
/// is simply "no applications in there", not an error worth ending the
/// listing over.
fn nested_applications(dir: &Path, parent: &str) -> Vec<ProjectRow> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && is_buildable(path))
        .map(|path| ProjectRow {
            name: format!(
                "{parent}/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            path,
            buildable: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ZephyrSettings;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chiptui-proj-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn input<'a>(home: &'a Path) -> ResolveInput<'a> {
        ResolveInput {
            project_settings: None,
            user_settings: None,
            home,
        }
    }

    fn app_dir(parent: &Path, name: &str, with_cmake: bool) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if with_cmake {
            std::fs::write(
                dir.join("CMakeLists.txt"),
                "find_package(Zephyr REQUIRED)\n",
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn project_config_outranks_user_config() {
        let tmp = scratch("levels");
        let pinned = app_dir(&tmp, "pinned", false);
        let fallback = app_dir(&tmp, "fallback", false);
        let project = ZephyrSettings {
            projects: Some(pinned.display().to_string()),
            ..Default::default()
        };
        let user = ZephyrSettings {
            projects: Some(fallback.display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.project_settings = Some(&project);
        input.user_settings = Some(&user);
        assert_eq!(resolve(&input), ProjectsResolution::Configured(pinned));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn without_config_the_answer_is_ask_not_guess() {
        let tmp = scratch("none");
        assert_eq!(resolve(&input(&tmp)), ProjectsResolution::NotConfigured);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_missing_location_is_rejected_with_the_key_named() {
        let tmp = scratch("missing");
        let user = ZephyrSettings {
            projects: Some(tmp.join("nowhere").display().to_string()),
            ..Default::default()
        };
        let mut input = input(&tmp);
        input.user_settings = Some(&user);
        let ProjectsResolution::Invalid(message) = resolve(&input) else {
            panic!("expected a rejection");
        };
        assert!(
            message.contains("[zephyr] projects"),
            "names the fix: {message}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn buildability_is_the_cmake_entry_point_and_nothing_else() {
        let tmp = scratch("buildable");
        let app = app_dir(&tmp, "app", true);
        let scratch_dir = app_dir(&tmp, "scratch", false);
        assert!(is_buildable(&app));
        assert!(!is_buildable(&scratch_dir));
        assert!(!is_buildable(&tmp.join("never-existed")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rows_list_every_subdirectory_sorted_and_marked() {
        let tmp = scratch("rows");
        app_dir(&tmp, "zephyr-app", true);
        app_dir(&tmp, "notes", false);
        std::fs::write(tmp.join("afile.txt"), "").unwrap();

        let (rows, error) = project_rows(&tmp);
        assert_eq!(error, None);
        let names: Vec<(&str, bool)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.buildable))
            .collect();
        assert_eq!(
            names,
            vec![("notes", false), ("zephyr-app", true)],
            "sorted by name, files excluded, buildability marked"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A module hook's `CMakeLists.txt` is a comment and nothing else ---
    /// `west build` refuses such a directory, so the gate must too.
    #[test]
    fn a_module_hook_is_not_an_application() {
        let tmp = scratch("modulehook");
        let repo = tmp.join("board-module");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("CMakeLists.txt"),
            "# The module contributes a board only (board_root in module.yml).\n\
             # No source files are added to the build.\n",
        )
        .unwrap();
        assert!(!is_buildable(&repo));
    }

    /// The out-of-tree board layout: the repository root is the module and
    /// the application is `app/`. The picker offers the application, not
    /// the root that cannot be built.
    #[test]
    fn an_application_one_level_down_is_listed_instead_of_its_module_root() {
        let tmp = scratch("nested");
        let repo = tmp.join("t-display");
        std::fs::create_dir_all(repo.join("boards/lilygo")).unwrap();
        std::fs::write(repo.join("CMakeLists.txt"), "# module hook\n").unwrap();
        app_dir(&repo, "app", true);

        let (rows, error) = project_rows(&tmp);
        assert_eq!(error, None);
        let names: Vec<(&str, bool)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.buildable))
            .collect();
        assert_eq!(names, vec![("t-display/app", true)]);
        assert_eq!(rows[0].path, repo.join("app"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A plain directory with nothing buildable under it stays one
    /// unbuildable row --- the honest inventory, not a vanished entry.
    #[test]
    fn a_directory_with_no_application_below_it_stays_a_single_row() {
        let tmp = scratch("nonested");
        let notes = tmp.join("notes");
        std::fs::create_dir_all(notes.join("drafts")).unwrap();

        let (rows, _) = project_rows(&tmp);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "notes");
        assert!(!rows[0].buildable);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_unreadable_directory_reports_rather_than_dead_ends() {
        let tmp = scratch("unreadable");
        let (rows, error) = project_rows(&tmp.join("nowhere"));
        assert!(rows.is_empty());
        assert!(error.is_some(), "the picker needs the reason");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
