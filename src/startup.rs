//! Where a run begins: the dashboard, or the home screen.
//!
//! `SPEC.md` §7. ChipTUI is project-aware, so starting it inside a project
//! must land straight in that project --- the home screen is for the case
//! where the working directory answers nothing. The decision is made here,
//! once, before the terminal is taken over, and it is a pure function of the
//! filesystem plus the recorded projects so it can be tested without a tty.

use std::path::{Path, PathBuf};

use crate::backend::BackendRegistry;
use crate::project::{DetectionOutcome, detect_from_known};
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
/// In order: a directory detection can name a backend for is opened, whether
/// the answer came from the project's `chiptui.toml`, from the registry, or
/// from the evidence itself; an *ambiguous* directory is opened too, so the
/// prompt that resolves it appears where the user already is; an empty
/// directory is opened so it can be scaffolded; anything else --- a
/// directory with contents and no project in it or above it, `$HOME` being
/// the usual one --- goes to the home screen.
///
/// A `start` that cannot be read is not a project either, so it routes to
/// the home screen rather than failing the run: the user can pick a project
/// from there.
pub fn route(start: &Path, backends: &BackendRegistry, known: &ProjectRegistry) -> Route {
    let Ok(detection) = detect_from_known(backends, start, known) else {
        return Route::Home;
    };
    match detection.outcome {
        DetectionOutcome::Detected(_) | DetectionOutcome::Ambiguous(_) => {
            Route::Open(start.to_path_buf())
        }
        DetectionOutcome::Unknown if is_empty_dir(start) => Route::Open(start.to_path_buf()),
        DetectionOutcome::Unknown => Route::Home,
    }
}

/// Whether `dir` holds nothing the user put there. Hidden entries do not
/// count: a freshly `git init`-ed directory is still an empty project, and
/// so is one carrying an editor's dotfile.
fn is_empty_dir(dir: &Path) -> bool {
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

    #[test]
    fn a_directory_with_contents_and_no_project_goes_home() {
        let dir = temp_dir("busy");
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        let route = route_for(&dir, &ProjectRegistry::default());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(route, Route::Home);
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

        assert_eq!(unknown, Route::Home, "unrecorded, it is just a directory");
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
