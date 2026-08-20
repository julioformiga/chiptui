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

    // Seams in place before `bootstrap`: the tool report inside it already
    // resolves the workspace, which must read this fixture home (pre-seeded
    // above), not the machine's real one.
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(&home);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
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

/// The checklist's `Project path` row --- it lives in the Project pane
/// (ctrl+p's checklist), two rows below the folder question: Enter opens
/// the project flow (the projects-folder question when nothing is
/// configured, the project picker when one is).
fn press_project_row(app: &mut App) {
    app.handle(AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        KeyCode::Char('p'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    )));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
}

/// The `Build` button --- fifth row of the panel's list (Update Zephyr,
/// SDK List, Menuconfig, Clean, Build, ...) now that the workspace pair
/// leads and the questions live in the workspace pane.
fn press_build(app: &mut App) {
    app.focus = Focus::Build;
    app.build.as_mut().unwrap().cursor = 4;
    app.handle(key(KeyCode::Enter));
}

fn log_mentions(app: &App, needle: &str) -> bool {
    app.logs
        .visible(usize::MAX)
        .any(|entry| entry.message.contains(needle))
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| chiptui::ui::draw(frame, app))
        .unwrap();
    terminal.backend().to_string()
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

    press_project_row(&mut app);
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

    press_project_row(&mut app);
    assert!(matches!(app.overlay, Some(Overlay::ProjectPicker { .. })));
    assert!(!app.build.as_ref().unwrap().is_busy());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_directory_without_build_elements_cannot_be_accepted() {
    let (mut app, root) = bare_app("reject", Some(&root_for("reject").join("apps")));
    app_dir(&root.join("apps"), "notes", false);

    press_project_row(&mut app); // the folder is configured: the project picker
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
    // A cached board in the picked project: the checklist's other half, so
    // the Build button is enabled once the pick lands.
    std::fs::create_dir_all(blinky.join("build/zephyr")).unwrap();
    std::fs::write(
        blinky.join("build/zephyr/CMakeCache.txt"),
        "CACHED_BOARD:STRING=nrf52840dk/nrf52840\n",
    )
    .unwrap();
    app_dir(&root.join("apps"), "notes", false);
    app.build.as_mut().unwrap().set_tool_path(fake("west"));

    // The header must not name a project that has not been chosen yet:
    // the cwd is not a project just because ChipTUI started in it.
    assert_eq!(app.header_project(), "");

    press_project_row(&mut app); // gate -> project picker
    app.handle(key(KeyCode::Enter)); // rows sorted: blinky first
    assert_eq!(app.overlay, None, "a buildable pick closes the picker");
    let panel = app.build.as_ref().unwrap();
    assert_eq!(
        panel.root, blinky,
        "every command now runs in the picked app"
    );
    assert_eq!(panel.project_origin, chiptui::build::ProjectOrigin::Picked);
    assert_eq!(
        app.header_project(),
        "blinky",
        "the header names the picked project's folder"
    );

    // The Project pane's path row follows the pick too (it would still
    // name the bare cwd otherwise).
    let frame = render(&mut app, 100, 32);
    let path_line = frame
        .lines()
        .find(|line| line.contains("Project path"))
        .expect("the path row must render");
    assert!(
        path_line.contains("blinky"),
        "the path row must name the picked project, got: {path_line}"
    );

    // The gate is satisfied by the pick (and its cached board): Build
    // runs, and succeeds.
    press_build(&mut app);
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

    // Project pane, unresolved: [Zephyr path, Projects base, ...] --- one
    // Down reaches the projects-folder row (ctrl+p lands on the first open
    // question, which the installation is).
    app.handle(AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        KeyCode::Char('p'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    )));
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
    // The checklist's other half: a cached board, so the Build button is
    // enabled for the working directory's own project.
    std::fs::create_dir_all(root.join("build/zephyr")).unwrap();
    std::fs::write(
        root.join("build/zephyr/CMakeCache.txt"),
        "CACHED_BOARD:STRING=nrf52840dk/nrf52840\n",
    )
    .unwrap();

    // Both seams in place before `bootstrap`: its tool report already
    // resolves the workspace, which must not read the machine's real
    // $HOME.
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
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

#[test]
fn a_project_switch_applies_the_new_projects_saved_board_and_shield() {
    let root = root_for("switch-board");
    let apps = root.join("apps");
    let (mut app, _root) = bare_app("switch-board", Some(&apps));
    let alpha = app_dir(&apps, "alpha", true);
    let beta = app_dir(&apps, "beta", true);

    // The registry already knows beta's target answers --- saved by a
    // earlier session's pickers. Written after `bare_app` seeded the
    // `[zephyr]` section, so the file carries both halves; the reload
    // makes the running app see it (detection would on the next start).
    let config = root.join("home/.config/chiptui/config.toml");
    std::fs::write(
        &config,
        format!(
            "[zephyr]\nprojects = \"{}\"\n\n[[project]]\npath = \"{}\"\nbackend = \"zephyr\"\nboard = \"thingy91/nrf9160\"\nshield = \"nrf7002ek\"\n",
            apps.display(),
            beta.display()
        ),
    )
    .unwrap();
    app.set_home_dir(root.join("home"));

    // Pick alpha first: no registry entry, no cache --- no board.
    press_project_row(&mut app);
    app.handle(key(KeyCode::Enter)); // rows sorted: alpha first
    assert_eq!(app.build.as_ref().unwrap().root, alpha);
    assert_eq!(app.build.as_ref().unwrap().board_name(), None);

    // Switch to beta: its saved answers apply, cache-independent. (The
    // pane's cursor never left the Project path row, and ctrl+p is a
    // toggle --- a second press would leave the pane.)
    app.handle(key(KeyCode::Enter)); // reopen the picker
    app.handle(key(KeyCode::Down)); // alpha -> beta
    app.handle(key(KeyCode::Enter));
    let panel = app.build.as_ref().unwrap();
    assert_eq!(panel.root, beta);
    assert_eq!(
        panel.board_name(),
        Some("thingy91/nrf9160"),
        "the new project's saved board applies on the switch"
    );
    assert_eq!(
        panel.board.as_ref().unwrap().origin,
        chiptui::build::BoardOrigin::Config
    );
    assert_eq!(panel.shield_name(), Some("nrf7002ek"));
    assert!(
        !beta.join("build/zephyr/CMakeCache.txt").exists(),
        "the answers come from the registry, never from a write into the project"
    );
    let _ = std::fs::remove_dir_all(&root);
}
