//! Where a run begins: the dashboard, or the home screen.
//!
//! `SPEC.md` §7. ChipTUI is project-aware, so starting it inside a project
//! must land straight in that project --- the home screen is for the case
//! where the working directory answers nothing. The decision is made here,
//! once, before the terminal is taken over, and it is a pure function of the
//! filesystem plus the recorded projects so it can be tested without a tty.

use std::path::{Path, PathBuf};

use crate::backend::BackendRegistry;
use crate::project::detect_from_known;
use crate::settings::ProjectRegistry;

/// Which screen the session opens on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Open the dashboard rooted at this directory. The directory need not
    /// be a project yet: an empty one opens with the backend prompt
    /// (`SPEC.md` §7), which is what makes `mkdir x && cd x && chiptui` work.
    Open(PathBuf),
    /// Nothing here to open --- list the recorded projects instead.
    Home,
}

/// Decides the opening screen for `start`.
///
/// A readable directory is **always** opened now, whatever detection made of
/// it: a project it can name outright, an ambiguous one, an empty one, and a
/// directory full of files it recognizes nothing in. The question the last
/// case used to have no answer for --- "this *is* a project, it just does
/// not look like one" --- is now a screen rather than a dead end, and the
/// screen is the dashboard's `Overlay::ProjectConfig` (`App::
/// maybe_open_project_config`), which writes the project's own
/// `chiptui.toml`. Leaving it unanswered is what goes to the home screen,
/// from inside the session, so the list is a way *out* of the question
/// rather than the only answer to it.
///
/// The case that forced this: a Zephyr repository whose root is an
/// out-of-tree board *module* --- the application one directory down --- has
/// no `find_package(Zephyr)` at the top to score, so it reaches 0.25 against
/// a 0.35 floor and reads as `Unknown`. It was a real project, opened in its
/// own root, that the app could only answer with a list of other projects.
///
/// A `start` that cannot be read is the one thing left that is not a
/// project, so it routes to the home screen rather than failing the run.
pub fn route(start: &Path, backends: &BackendRegistry, known: &ProjectRegistry) -> Route {
    match detect_from_known(backends, start, known) {
        Ok(_) => Route::Open(start.to_path_buf()),
        Err(_) => Route::Home,
    }
}

/// Whether `dir` holds nothing the user put there. Hidden entries do not
/// count: a freshly `git init`-ed directory is still an empty project, and
/// so is one carrying an editor's dotfile.
///
/// Shared with `App::apply_project_type`, which scaffolds a backend's
/// starting layout only into such a directory: writing `CMakeLists.txt` into
/// a repository that already holds one is not what the scaffold is for.
pub(crate) fn is_empty_dir(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    !entries
        .flatten()
        .any(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendKind;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-startup-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn route_for(dir: &Path, known: &ProjectRegistry) -> Route {
        route(dir, &BackendRegistry::with_builtin_backends(), known)
    }

    fn registry_with(entries: &[(&Path, BackendKind)]) -> ProjectRegistry {
        let text: String = entries
            .iter()
            .map(|(path, backend)| {
                format!(
                    "[[project]]\npath = \"{}\"\nbackend = \"{}\"\n\n",
                    path.display(),
                    backend.id()
                )
            })
            .collect();
        ProjectRegistry::parse(&text)
    }

    /// The one directory that is still not a project: one that cannot be
    /// read at all. There is nothing for the configuration screen to write
    /// into, so the session falls back to the list.
    #[test]
    fn an_unreadable_directory_goes_home() {
        let dir = temp_dir("gone");
        let missing = dir.join("not-here");
        let route = route_for(&missing, &ProjectRegistry::default());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(route, Route::Home);
    }

    #[test]
    fn an_empty_directory_opens_so_it_can_be_scaffolded() {
        let dir = temp_dir("empty");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let route = route_for(&dir, &ProjectRegistry::default());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            route,
            Route::Open(dir),
            "hidden entries do not make a directory non-empty"
        );
    }

    /// The regression this routing exists for: a Zephyr repository whose
    /// root is an out-of-tree board module, the application one level down.
    /// Nothing at the top calls `find_package(Zephyr)`, so it scores 0.25
    /// against a 0.35 floor and reads as `Unknown` --- and it used to be
    /// answered with the home screen's list of *other* projects.
    #[test]
    fn a_directory_with_contents_and_no_project_still_opens() {
        let dir = temp_dir("busy");
        std::fs::create_dir_all(dir.join("boards")).unwrap();
        std::fs::create_dir_all(dir.join("zephyr")).unwrap();
        std::fs::write(dir.join("CMakeLists.txt"), "# a module hook only\n").unwrap();
        std::fs::write(dir.join("Kconfig"), "").unwrap();
        std::fs::write(dir.join("zephyr/module.yml"), "name: board\n").unwrap();
        let route = route_for(&dir, &ProjectRegistry::default());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            route,
            Route::Open(dir),
            "the configuration screen answers this, not the project list"
        );
    }

    #[test]
    fn a_registered_directory_opens_without_any_marker_file() {
        let dir = temp_dir("registered");
        // The MicroPython scaffold's own shape: nothing at the root scores,
        // so only the registry can identify it.
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.py"), "print('hi')\n").unwrap();

        let known = registry_with(&[(dir.as_path(), BackendKind::MicroPython)]);
        let unknown = route_for(&dir, &ProjectRegistry::default());
        let registered = route_for(&dir, &known);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            unknown,
            Route::Open(dir.clone()),
            "unrecorded it still opens --- the screen that names it is inside"
        );
        assert_eq!(registered, Route::Open(dir));
    }

    #[test]
    fn a_subdirectory_of_a_project_opens_the_project() {
        let dir = temp_dir("nested");
        let inner = dir.join("src/drivers");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();

        let known = registry_with(&[(dir.as_path(), BackendKind::Zephyr)]);
        let route = route_for(&inner, &known);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            route,
            Route::Open(inner),
            "the dashboard starts where the user is; detection finds the root above"
        );
    }

    #[test]
    fn a_detected_project_opens_without_being_registered() {
        let dir = temp_dir("detected");
        std::fs::write(
            dir.join("CMakeLists.txt"),
            "find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})\n",
        )
        .unwrap();
        std::fs::write(dir.join("prj.conf"), "").unwrap();

        let route = route_for(&dir, &ProjectRegistry::default());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(route, Route::Open(dir));
    }

    #[test]
    fn the_zephyr_scaffold_is_recognized_on_its_own_evidence() {
        let dir = temp_dir("scaffolded");
        let backends = BackendRegistry::with_builtin_backends();
        let scaffold = backends
            .get(BackendKind::Zephyr)
            .unwrap()
            .scaffold("blinky");
        crate::project::scaffold::create(&dir, &scaffold).unwrap();

        let route = route_for(&dir, &ProjectRegistry::default());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            route,
            Route::Open(dir),
            "a scaffolded Zephyr app must not depend on the registry to be found"
        );
    }
}
