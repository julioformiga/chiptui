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
//! flash, menuconfig) runs, its working directory must hold build elements
//! or have one resolved inside it. A directory that fails is never built
//! silently --- the picker lists only what can run ([`project_rows`]), and
//! an empty listing says why.

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
    /// Whether the row can be accepted (every listed row can --- this is
    /// the accept path's contract, not a display fact).
    pub buildable: bool,
    /// Why the row is listed at all: the build entry point it holds, as a
    /// path relative to the row's own directory --- `CMakeLists.txt` for an
    /// application, `app/CMakeLists.txt` for a repository whose single
    /// application sits one level down. The picker shows it beside the
    /// name, so the user reads *where* the build would run from.
    pub evidence: String,
}

/// The project picker's rows for `dir`: the subdirectories a build can run
/// in, sorted by name. A directory with no application under it is not
/// listed --- the picker offers choices, not an inventory, and the dimmed
/// not-buildable rows it used to carry answered no question. Nothing
/// qualifying is a *message* (`Some`), so the picker says why it is empty
/// rather than implying an empty folder; a folder with no subdirectories at
/// all stays message-less (the picker has its own line for that), and a
/// directory that cannot be read reports why, so the picker never
/// dead-ends.
///
/// A subdirectory that is not itself an application is searched **one level
/// deeper**. One buildable child makes the *repository* the row (accepted
/// as the root, the application resolving as its source directory ---
/// `resolve_app`); several become `parent/child` rows, each a project of
/// its own. That is not a general recursive walk: it is the one extra
/// level the out-of-tree board layout costs, where the repository is the
/// module and the application lives beside the `boards/` tree it uses.
pub fn project_rows(dir: &Path) -> (Vec<ProjectRow>, Option<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => return (Vec::new(), Some(format!("{}: {err}", dir.display()))),
    };
    let mut rows: Vec<ProjectRow> = Vec::new();
    let mut had_dirs = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        had_dirs = true;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_buildable(&path) {
            rows.push(ProjectRow {
                name,
                path,
                buildable: true,
                evidence: BUILD_ENTRY.to_string(),
            });
            continue;
        }
        let nested = nested_applications(&path, &name);
        if nested.len() == 1 {
            // One application inside: the repository itself is the project
            // (accepted as the root, the application resolving as its
            // source directory --- `resolve_app`), not a `parent/child`
            // row that would re-root into the child and strand the
            // repository's `build/` directories one level above it. The
            // evidence names *where* the entry point really is, because the
            // repository's own `CMakeLists.txt` is a module hook.
            let child = nested[0]
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            rows.push(ProjectRow {
                name,
                path,
                buildable: true,
                evidence: format!("{child}/{BUILD_ENTRY}"),
            });
        } else {
            rows.extend(nested);
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.dedup_by(|a, b| a.name == b.name);
    if rows.is_empty() && had_dirs {
        return (
            Vec::new(),
            Some(format!(
                "no Zephyr application in {} — every folder lacks a CMakeLists.txt with find_package(Zephyr)",
                dir.display()
            )),
        );
    }
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
            evidence: BUILD_ENTRY.to_string(),
        })
        .collect()
}

/// The application a directory *entered from* resolves to, when it resolves
/// to exactly one: a directory that is not itself an application but holds
/// exactly one buildable direct subdirectory --- the repository whose root
/// is an out-of-tree board module and whose application sits one level down
/// (`app/` by Zephyr convention, but any name answers, provided the child
/// is the only application in there).
///
/// Uniqueness is the whole bar: two buildable children are a choice, and a
/// choice is the picker's to present ([`project_rows`]), never a resolution
/// to make silently. A directory that is itself buildable answers `None`
/// --- it needs no resolution, and its children are not ChipTUI's to rank.
pub fn entry_child(dir: &Path) -> Option<PathBuf> {
    if is_buildable(dir) {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut children = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && is_buildable(path));
    let only = children.next()?;
    children.next().is_none().then_some(only)
}

/// Where a project root's application lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppSource {
    /// The root is itself the application.
    Root,
    /// A directory inside it: declared by `[zephyr] app` in the root's
    /// `chiptui.toml`, or the single buildable direct subdirectory
    /// ([`entry_child`]).
    Dir(PathBuf),
}

/// The application a project root builds, west's source directory: the
/// root itself when it is buildable; otherwise the directory named by
/// `[zephyr] app` in the root's `chiptui.toml` (a path relative to the
/// root); otherwise the root's single buildable direct subdirectory.
///
/// A declared key outranks the discovery, and a declared key that does not
/// name a buildable directory answers `None` outright --- an explicit
/// answer that no longer holds is reported as such, never silently
/// replaced by a guess. This is the root-as-project model: nothing
/// re-roots; `west build` runs in the root with the application as its
/// source-directory argument, so the repository's `build/` directories
/// stay where the repository keeps them.
pub fn resolve_app(root: &Path) -> Option<AppSource> {
    if is_buildable(root) {
        return Some(AppSource::Root);
    }
    if let Some(app) = declared_app(root) {
        return is_buildable(&app).then_some(AppSource::Dir(app));
    }
    entry_child(root).map(AppSource::Dir)
}

/// The application directory the project's own `chiptui.toml` declares
/// (`[zephyr] app`, relative to the project root). None when the file or
/// the key is absent.
pub fn declared_app(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root.join(crate::project::config::FILE_NAME)).ok()?;
    let rel =
        crate::project::config::key_value(&text, crate::project::config::ZEPHYR_SECTION, "app")?;
    Some(root.join(rel))
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
    fn a_repo_with_one_application_resolves_to_it_whatever_its_name() {
        let tmp = scratch("entry");
        // The out-of-tree board module shape: a root `CMakeLists.txt` that
        // is a module hook (no `find_package`), a `boards/` tree, and the
        // application one level down.
        std::fs::write(
            tmp.join("CMakeLists.txt"),
            "# No sources; this module contributes boards only.\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.join("boards")).unwrap();
        let app = app_dir(&tmp, "app", true);
        std::fs::create_dir_all(tmp.join("dts")).unwrap();
        assert_eq!(entry_child(&tmp), Some(app));
        // Any name answers, not just the conventional `app/`.
        let other = scratch("entry-any-name");
        let src = app_dir(&other, "firmware", true);
        assert_eq!(entry_child(&other), Some(src));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn two_applications_are_a_choice_not_a_resolution() {
        let tmp = scratch("entry-two");
        app_dir(&tmp, "app", true);
        app_dir(&tmp, "sample", true);
        assert_eq!(entry_child(&tmp), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_application_root_and_a_directory_without_one_resolve_to_nothing() {
        let tmp = scratch("entry-none");
        app_dir(&tmp, "app", true);
        assert_eq!(entry_child(&tmp.join("app")), None);
        app_dir(&tmp, "notes", false);
        std::fs::create_dir_all(tmp.join("empty")).unwrap();
        assert_eq!(entry_child(&tmp.join("empty")), None);
        assert_eq!(entry_child(&tmp.join("never-existed")), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_buildable_root_is_its_own_application() {
        let tmp = scratch("resolve-root");
        app_dir(&tmp, "app", true);
        std::fs::write(
            tmp.join("CMakeLists.txt"),
            "find_package(Zephyr REQUIRED)\n",
        )
        .unwrap();
        assert_eq!(resolve_app(&tmp), Some(AppSource::Root));
        // Even with a declared key: a root holding a `find_package` is the
        // answer west would use anyway.
        std::fs::write(tmp.join("chiptui.toml"), "[zephyr]\napp = \"app\"\n").unwrap();
        assert_eq!(resolve_app(&tmp), Some(AppSource::Root));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_declared_application_outranks_the_discovery() {
        let tmp = scratch("resolve-declared");
        app_dir(&tmp, "app", true);
        let firmware = app_dir(&tmp, "firmware", true);
        std::fs::write(tmp.join("chiptui.toml"), "[zephyr]\napp = \"firmware\"\n").unwrap();
        assert_eq!(
            resolve_app(&tmp),
            Some(AppSource::Dir(firmware)),
            "the key names the application, not the first child found"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_declared_application_that_no_longer_builds_is_not_replaced() {
        let tmp = scratch("resolve-broken");
        // The discovery alone would answer `app`; the declared key says
        // otherwise, and a broken explicit answer is never silently
        // swapped for a guess.
        app_dir(&tmp, "app", true);
        app_dir(&tmp, "gone", false);
        std::fs::write(tmp.join("chiptui.toml"), "[zephyr]\napp = \"gone\"\n").unwrap();
        assert_eq!(declared_app(&tmp), Some(tmp.join("gone")));
        assert_eq!(resolve_app(&tmp), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_undeclared_single_application_resolves_by_uniqueness() {
        let tmp = scratch("resolve-unique");
        let app = app_dir(&tmp, "app", true);
        assert_eq!(resolve_app(&tmp), Some(AppSource::Dir(app)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rows_list_the_applications_only() {
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
            vec![("zephyr-app", true)],
            "a folder with no application is not a choice and is not listed"
        );
        assert_eq!(
            rows[0].evidence, "CMakeLists.txt",
            "an application's entry point is its own"
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
    /// the application is `app/`. The picker offers the *repository* as one
    /// acceptable row (the root stays the project; the application resolves
    /// inside it) --- not a `parent/child` row that would re-root into the
    /// child and strand the repository's `build/` directories.
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
        assert_eq!(names, vec![("t-display", true)]);
        assert_eq!(rows[0].path, repo);
        assert_eq!(
            rows[0].evidence, "app/CMakeLists.txt",
            "the row says where the entry point really is — the root's own CMakeLists is a module hook"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A plain directory with nothing buildable under it is not listed,
    /// and the emptiness says why --- folders exist but none is a project.
    #[test]
    fn a_directory_with_no_application_below_it_is_not_listed_but_explained() {
        let tmp = scratch("nonested");
        let notes = tmp.join("notes");
        std::fs::create_dir_all(notes.join("drafts")).unwrap();

        let (rows, reason) = project_rows(&tmp);
        assert!(rows.is_empty(), "not a choice, not a row");
        let reason = reason.expect("the emptiness must explain itself");
        assert!(reason.contains("CMakeLists.txt"), "names the bar: {reason}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A folder with no subdirectories at all is empty without inventing a
    /// reason --- the picker has its own line for that shape.
    #[test]
    fn an_empty_folder_is_empty_without_inventing_a_reason() {
        let tmp = scratch("nodirs");
        let (rows, reason) = project_rows(&tmp);
        assert!(rows.is_empty());
        assert_eq!(reason, None);
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
