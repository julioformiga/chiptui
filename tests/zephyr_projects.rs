//! The Zephyr projects flow end to end: the projects folder (configured or
//! picked, persisted like the installation), the project picker with its
//! build-element verification, and the gate that refuses to run build/clean
//! commands in a directory that is not a Zephyr application (`SPEC.md` §8's
//! never-guess rule, applied to *what* is built).

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::app::{App, Focus, Overlay};
use chiptui::backend::BackendKind;
use chiptui::event::AppEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn fake(tool: &str) -> String {
    format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// The temp root `bare_app` will use for `tag` (the same formula, so a
/// test can pre-compute paths inside it).
fn root_for(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("chiptui-projs-{tag}-{}", std::process::id()))
}

/// A Zephyr backend selected in a directory that is NOT a project --- the
/// flow's whole reason: ChipTUI launched from anywhere, the project chosen
/// inside it. `home` holds a fixture user config (pre-seeded with a
/// `projects` key when `projects` names a folder), `apps` is a folder of
/// candidate projects.
fn bare_app(tag: &str, projects: Option<&std::path::Path>) -> (App, std::path::PathBuf) {
    let root = root_for(tag);
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(root.join("apps")).unwrap();
    std::fs::create_dir_all(root.join("dev")).unwrap();
    if let Some(dir) = projects {
        let config = home.join(".config/chiptui/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            format!("[zephyr]\nprojects = \"{}\"\n", dir.display()),
        )
        .unwrap();
    }

    let mut app = App::new(&root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(&home);
    app.maybe_scan_devices();
    (app, root)
}

/// An app fixture inside `parent`: a directory with (or without) the
/// `CMakeLists.txt` that makes it buildable.
fn app_dir(parent: &std::path::Path, name: &str, with_cmake: bool) -> std::path::PathBuf {
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

/// The first row of the build panel's list: `Build`.
fn press_build(app: &mut App) {
    app.focus = Focus::Build;
    app.handle(key(KeyCode::Enter));
}

fn log_mentions(app: &App, needle: &str) -> bool {
    app.logs
        .visible(usize::MAX)
        .any(|entry| entry.message.contains(needle))
}

/// Drains process events into the app until the build panel reports a
/// finished command.
fn pump_build(app: &mut App, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        if app.build.as_ref().unwrap().last.is_some() {
            return true;
        }
        app.handle(AppEvent::Tick);
        std::thread::sleep(Duration::from_millis(5));
    }
    app.build.as_ref().unwrap().last.is_some()
}

#[test]
fn build_outside_a_project_refuses_and_asks_for_the_folder_first() {
    let (mut app, root) = bare_app("gate-none", None);

    press_build(&mut app);
    assert!(
        !app.build.as_ref().unwrap().is_busy(),
        "nothing may run in a directory without build elements"
    );
    assert!(
        app.processes.drain().is_empty(),
        "not even a subprocess was spawned"
    );
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::DirPicker {
                purpose: chiptui::workspace::DirPurpose::Projects,
                ..
            })
        ),
        "the projects-folder question comes first, got {:?}",
        app.overlay
    );
    assert!(
        log_mentions(&app, "pick a project first"),
        "the refusal explains itself in the log"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn with_a_folder_configured_the_gate_opens_the_project_picker_instead() {
    let (mut app, root) = bare_app("gate-folder", Some(&root_for("gate-folder").join("apps")));
    assert_eq!(
        app.workspace.as_ref().unwrap().projects,
        Some(root.join("apps")),
        "the pane resolved the folder from the user config at creation"
    );

    press_build(&mut app);
    assert!(matches!(app.overlay, Some(Overlay::ProjectPicker { .. })));
    assert!(!app.build.as_ref().unwrap().is_busy());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_directory_without_build_elements_cannot_be_accepted() {
    let (mut app, root) = bare_app("reject", Some(&root_for("reject").join("apps")));
    app_dir(&root.join("apps"), "notes", false);

    press_build(&mut app); // the folder is configured: the project picker
    assert!(matches!(app.overlay, Some(Overlay::ProjectPicker { .. })));

    app.handle(key(KeyCode::Enter)); // the only row: notes (not buildable)
    let Some(Overlay::ProjectPicker {
        error: Some(reason),
        ..
    }) = app.overlay
    else {
        panic!(
            "the picker must stay open with the reason, got {:?}",
            app.overlay
        );
    };
    assert!(
        reason.contains("CMakeLists.txt"),
        "names the missing element: {reason}"
    );
    assert!(!app.build.as_ref().unwrap().is_busy());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn picking_a_buildable_project_reroots_and_builds() {
    let (mut app, root) = bare_app("pick", Some(&root_for("pick").join("apps")));
    let blinky = app_dir(&root.join("apps"), "blinky", true);
    app_dir(&root.join("apps"), "notes", false);
    app.build.as_mut().unwrap().set_tool_path(fake("west"));

    press_build(&mut app); // gate -> project picker
    app.handle(key(KeyCode::Enter)); // rows sorted: blinky first
    assert_eq!(app.overlay, None, "a buildable pick closes the picker");
    let panel = app.build.as_ref().unwrap();
    assert_eq!(
        panel.root, blinky,
        "every command now runs in the picked app"
    );
    assert_eq!(panel.project_origin, chiptui::build::ProjectOrigin::Picked);

    // The gate is satisfied by the pick: Build runs, and succeeds.
    app.handle(key(KeyCode::Enter));
    assert!(app.build.as_ref().unwrap().is_busy());
    assert!(pump_build(&mut app, 10));
    assert!(
        app.build.as_ref().unwrap().last.as_ref().unwrap().ok,
        "the fake west succeeds in the picked project"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn choosing_the_folder_in_the_picker_persists_it_and_chains_to_the_project() {
    let (mut app, root) = bare_app("persist", None);
    let apps = root.join("apps");
    app_dir(&apps, "blinky", true);

    // Workspace pane, unresolved: [Choose, Projects] --- one Down reaches
    // the projects-folder chooser.
    app.focus = Focus::Workspace;
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::DirPicker {
            purpose: chiptui::workspace::DirPurpose::Projects,
            ..
        })
    ));

    // The picker starts at the fixture home (empty). Climb to the root
    // (`..`), then descend into `apps` (the first subdirectory there after
    // "use" and ".."), where the reflex Enter accepts the folder.
    app.handle(key(KeyCode::Down)); // ".."
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Down)); // root's ".."
    app.handle(key(KeyCode::Down)); // apps
    app.handle(key(KeyCode::Enter)); // descend (lands on "use this directory")
    app.handle(key(KeyCode::Enter)); // accept

    let config = root.join("home/.config/chiptui/config.toml");
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(
        saved.contains(&format!("projects = \"{}\"", apps.display())),
        "the pick is persisted where resolution reads it:\n{saved}"
    );
    assert_eq!(app.workspace.as_ref().unwrap().projects, Some(apps));
    assert_eq!(
        app.build.as_ref().unwrap().root,
        root,
        "the folder alone re-roots nothing"
    );
    assert!(
        matches!(app.overlay, Some(Overlay::ProjectPicker { .. })),
        "the project question follows the folder"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_cwd_that_is_already_a_project_never_asks() {
    let root = std::env::temp_dir().join(format!("chiptui-projs-cwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();

    let mut app = App::new(&root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.maybe_scan_devices();
    app.build.as_mut().unwrap().set_tool_path(fake("west"));

    assert_eq!(app.workspace.as_ref().unwrap().projects, None);
    press_build(&mut app);
    assert!(
        app.build.as_ref().unwrap().is_busy(),
        "the working directory's own build elements satisfy the gate"
    );
    let _ = std::fs::remove_dir_all(&root);
}
