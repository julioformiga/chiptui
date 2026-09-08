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
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{enter_project_pane, fake, key, log_mentions, pump_until, render};

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
/// (the environment checklist), two rows below the folder question: Enter
/// opens the project flow (the projects-folder question when nothing is
/// configured, the project picker when one is).
fn press_project_row(app: &mut App) {
    enter_project_pane(app);
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
fn a_directory_without_build_elements_is_not_listed_and_the_emptiness_warns() {
    let (mut app, root) = bare_app("reject", Some(&root_for("reject").join("apps")));
    app_dir(&root.join("apps"), "notes", false);

    press_project_row(&mut app); // the folder is configured: the project picker
    assert!(matches!(app.overlay, Some(Overlay::ProjectPicker { .. })));

    // The picker warns in its own footer: folders exist, none is a project.
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("no Zephyr application"),
        "the warning must render in the picker:\n{frame}"
    );
    assert!(
        !frame.contains("notes"),
        "a folder with no application is not a row:\n{frame}"
    );

    // Enter on the empty list keeps the picker open with the reason.
    app.handle(key(KeyCode::Enter));
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
    // Down reaches the projects-folder row (entering the pane lands on the
    // first open question, which the installation is).
    enter_project_pane(&mut app);
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
    // pane's cursor never left the Project path row, and the `e` letter is
    // a no-op while the pane already holds focus.)
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

/// The out-of-tree board layout in the picker: the repository root is a
/// Zephyr *module* whose `CMakeLists.txt` adds no sources, so it is not
/// buildable and `west build` would refuse it. The application one level
/// down is what the picker must offer --- and picking it re-roots the
/// lifecycle there.
#[test]
fn the_project_picker_reaches_an_application_inside_a_board_module() {
    let (mut app, root) = bare_app("nested", None);
    let projects = root.join("projects");
    std::fs::create_dir_all(&projects).unwrap();

    // A plain application beside the module repository.
    app_dir(&projects, "blinky", true);

    // The module: a comment-only CMakeLists (west refuses it), a board
    // tree, and the real application in `app/`.
    let repo = projects.join("t-display");
    std::fs::create_dir_all(repo.join("boards/lilygo")).unwrap();
    std::fs::write(
        repo.join("CMakeLists.txt"),
        "# The module contributes a board only. No source files are added.\n",
    )
    .unwrap();
    app_dir(&repo, "app", true);

    let (rows, error) = chiptui::backend::zephyr::projects::project_rows(&projects);
    assert_eq!(error, None);
    let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["blinky", "t-display"],
        "one application inside: the repository itself is the row, not parent/child"
    );
    assert!(rows.iter().all(|row| row.buildable));
    assert_eq!(
        rows[1].path, repo,
        "accepting it keeps the repository as the project"
    );

    // And the gate agrees: the module root is not a place a build runs.
    assert!(!chiptui::backend::zephyr::projects::is_buildable(&repo));
    assert!(chiptui::backend::zephyr::projects::is_buildable(
        &repo.join("app")
    ));

    let _ = std::fs::remove_dir_all(&root);
    let _ = app.build.take();
}

/// A Zephyr session started *in* the repository: the start directory is a
/// board module (comment-only `CMakeLists.txt`, a `boards/` tree) and the
/// application sits one level down in `app/`. `toml` seeds the repository's
/// own `chiptui.toml` when given.
fn repo_app(tag: &str, toml: Option<&str>) -> (App, std::path::PathBuf) {
    let root = root_for(tag);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("boards/lilygo")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "# The module contributes a board only. No source files are added.
",
    )
    .unwrap();
    app_dir(&root, "app", true);
    if let Some(body) = toml {
        std::fs::write(root.join("chiptui.toml"), body).unwrap();
    }
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.maybe_scan_devices();
    (app, root)
}

/// Walks to the Board picker's row and picks the `nrf52840dk` target the
/// fake `west` lists, driving the real picker path (the background fetch,
/// the filter, the Enter) so the persist side runs as the app runs it.
fn pick_nrf_board(app: &mut App) {
    app.build.as_mut().unwrap().set_tool_path(fake("west"));
    enter_project_pane(app);
    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::BoardPicker { .. })));
    let loaded = pump_until(
        app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().boards.state,
                chiptui::build::ListState::Loaded(_)
            )
        },
        10,
    );
    assert!(loaded, "the fake west boards never finished");
    for ch in ['n', 'r', 'f'] {
        app.handle(key(KeyCode::Char(ch)));
    }
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.build.as_ref().unwrap().board_name(),
        Some("nrf52840dk/nrf52840"),
        "the filtered row's pick must land"
    );
}

/// Entering ChipTUI from a repository whose application sits one level down
/// asks the project question with the answer one `Enter` away: the picker
/// lists the *entered* directory (not the configured projects folder), the
/// cursor already on the only application in it. Accepting sets the
/// application directory --- the repository stays the project, so its
/// `build/` directories and its `chiptui.toml` stay where they are.
#[test]
fn entering_a_module_repo_asks_for_its_only_application_preselected() {
    let (mut app, root) = repo_app("entry-ask", None);
    app.maybe_open_entry_project();

    // The row says why it is there: the build entry point the folder holds.
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("app") && frame.contains("✓ CMakeLists.txt"),
        "the listed row names its evidence:\n{frame}"
    );

    let Some(Overlay::ProjectPicker {
        mpy: false,
        dir: Some(listed),
        selected,
        error: None,
    }) = app.overlay.take()
    else {
        panic!("the entry question must open, got {:?}", app.overlay);
    };
    assert_eq!(listed, root, "the picker lists the directory entered from");
    let (rows, _) = chiptui::backend::zephyr::projects::project_rows(&root);
    assert_eq!(
        rows[selected].path,
        root.join("app"),
        "the cursor starts on the application"
    );

    // One Enter applies it as the *application*: the root stays the project.
    app.overlay = Some(Overlay::ProjectPicker {
        mpy: false,
        dir: Some(listed),
        selected,
        error: None,
    });
    app.handle(key(KeyCode::Enter));
    let panel = app.build.as_ref().unwrap();
    assert_eq!(panel.root, root, "the repository stays the project root");
    assert_eq!(panel.app_dir.as_deref(), Some(root.join("app").as_path()));
    assert_eq!(
        panel.project_origin,
        chiptui::build::ProjectOrigin::WorkingDir
    );
    assert!(app.project_gate_ok(), "the gate passes with an application");
    assert!(log_mentions(&app, "application set to"));
    let _ = std::fs::remove_dir_all(&root);
}

/// With the application confirmed, the build command runs in the root and
/// names the application directory as west's source argument --- the exact
/// shape a hand-run `west build app` has from that repository.
#[test]
fn the_build_runs_in_the_root_with_the_app_as_its_source() {
    let (mut app, root) = repo_app("entry-command", None);
    app.maybe_open_entry_project();
    app.handle(key(KeyCode::Enter));

    let backend = app.manager.backend().unwrap();
    let command = app
        .build
        .as_ref()
        .unwrap()
        .command(chiptui::backend::BuildKind::Build, backend)
        .expect("the gate passed, the command must compose");
    let text = command.to_string();
    assert!(
        text.trim_end().ends_with(" app"),
        "the application rides as west's source argument: {text}"
    );
    assert_eq!(
        command.cwd(),
        Some(&root),
        "west runs in the repository, where build/ lives"
    );
    // The reports and menuconfig run against the build directory alone ---
    // no source argument on a `-t` invocation.
    let menuconfig = app
        .build
        .as_ref()
        .unwrap()
        .menuconfig_command(backend)
        .expect("menuconfig composes");
    assert!(!menuconfig.to_string().contains(" app "));
    let _ = std::fs::remove_dir_all(&root);
}

/// A repository that declares its application in its own `chiptui.toml`
/// (`[zephyr] app`) never asks: the file answered, and the pin beside it
/// rides along as always.
#[test]
fn a_declared_application_never_asks_and_resolves_silently() {
    let toml = "project_type = \"zephyr\"\n\n[zephyr]\napp = \"app\"\nboard = \"xiao_esp32c3\"\n";
    let (mut app, root) = repo_app("entry-declared", Some(toml));

    app.maybe_open_entry_project();
    assert!(
        app.overlay.is_none(),
        "the file answered; there is no question to ask"
    );
    let panel = app.build.as_ref().unwrap();
    assert_eq!(panel.root, root);
    assert_eq!(panel.app_dir.as_deref(), Some(root.join("app").as_path()));
    assert_eq!(
        panel.board_name(),
        Some("xiao_esp32c3"),
        "the pin is read from the repository's own chiptui.toml"
    );
    assert_eq!(
        panel.board.as_ref().unwrap().origin,
        chiptui::build::BoardOrigin::ProjectFile
    );
    assert!(log_mentions(&app, "application from chiptui.toml"));
    let _ = std::fs::remove_dir_all(&root);
}

/// A declared key that no longer names an application is named in the log
/// and replaced by nothing --- an explicit answer that stopped holding is a
/// fact to report, never a guess to fall back from.
#[test]
fn a_declared_application_that_no_longer_builds_is_named_not_replaced() {
    let toml = "project_type = \"zephyr\"\n\n[zephyr]\napp = \"gone\"\n";
    let (mut app, root) = repo_app("entry-broken", Some(toml));

    app.maybe_open_entry_project();
    assert!(
        app.overlay.is_none(),
        "the declared answer outranks the discovery, even broken"
    );
    let panel = app.build.as_ref().unwrap();
    assert_eq!(panel.app_dir, None, "nothing is resolved around it");
    assert!(
        !app.project_gate_ok(),
        "a broken declaration does not open the gate"
    );
    assert!(
        log_mentions(&app, "[zephyr] app"),
        "the log names the broken key"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Uniqueness is the bar: two applications in the entered directory are a
/// choice, and a choice is never resolved silently --- nothing opens at
/// startup, and the gate asks the configured-folder question instead of
/// guessing between the two.
#[test]
fn two_applications_in_the_entry_dir_are_a_choice_not_a_question() {
    let (mut app, root) = repo_app("entry-two", None);
    app_dir(&root, "sample", true);

    app.maybe_open_entry_project();
    assert!(
        app.overlay.is_none(),
        "nothing may be resolved when there is a choice to make"
    );

    press_project_row(&mut app);
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::DirPicker {
                purpose: chiptui::workspace::DirPurpose::Projects,
                ..
            })
        ),
        "the folder question, not a guessed listing: {:?}",
        app.overlay
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The target pick's destination rule against a repository project: a
/// present `chiptui.toml` at the root receives the board/shield answer
/// beside the registry entry for that same root --- the two halves of the
/// answer keyed by the one project.
#[test]
fn the_target_pick_writes_the_repositorys_own_chiptui_and_the_registry() {
    let (mut app, root) = repo_app("entry-toml", Some("project_type = \"zephyr\"\n"));
    app.maybe_open_entry_project();
    app.handle(key(KeyCode::Enter)); // the preselected application

    pick_nrf_board(&mut app);

    let written = std::fs::read_to_string(root.join("chiptui.toml")).unwrap();
    assert!(
        written.contains("board = \"nrf52840dk/nrf52840\""),
        "the pick is written to the repository's file:\n{written}"
    );
    assert!(
        written.contains("project_type = \"zephyr\""),
        "the file's other answers survive the write:\n{written}"
    );
    let entry = app
        .manager
        .known_projects()
        .entry_for(&root)
        .expect("the registry carries the answer for the repository root");
    assert_eq!(entry.board.as_deref(), Some("nrf52840dk/nrf52840"));
    let _ = std::fs::remove_dir_all(&root);
}

/// Without a `chiptui.toml` the same pick lands only in the user config's
/// registry --- the machine's memory of the project --- and no file is
/// invented to receive it.
#[test]
fn without_a_chiptui_the_entry_pick_stays_in_the_user_config() {
    let (mut app, root) = repo_app("entry-no-toml", None);
    app.maybe_open_entry_project();
    app.handle(key(KeyCode::Enter));

    pick_nrf_board(&mut app);

    assert!(
        !root.join("chiptui.toml").exists(),
        "a pick must not invent a project file"
    );
    let entry = app
        .manager
        .known_projects()
        .entry_for(&root)
        .expect("the registry carries the answer alone");
    assert_eq!(entry.board.as_deref(), Some("nrf52840dk/nrf52840"));
    let _ = std::fs::remove_dir_all(&root);
}
