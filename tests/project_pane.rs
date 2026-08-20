//! The Project pane (row 1): `ctrl+p`'s way in and out, the MicroPython
//! project questions (projects folder, project pick, dependencies report,
//! script report), and the re-rooting a pick performs on the file browser's
//! local side. The Zephyr rows' flows are covered by `build_view.rs`; this
//! file covers the pane-level grammar and the MicroPython half.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use chiptui::app::{App, Focus, Overlay};
use chiptui::backend::BackendKind;
use chiptui::event::AppEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| chiptui::ui::draw(frame, app))
        .unwrap();
    terminal.backend().to_string()
}

/// A scratch tree with an isolated home (`home/`) and serial seam (`dev/`)
/// so nothing on the machine running the tests leaks in.
fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("chiptui-projpane-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("dev")).unwrap();
    root
}

/// A MicroPython app: `main.py` at the root (so the project detects as one),
/// seams pointed at the scratch tree.
fn mpy_app(tag: &str) -> (App, PathBuf) {
    let root = scratch(tag);
    std::fs::write(root.join("main.py"), "print('hi')\n").unwrap();
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    (app, root)
}

/// A Zephyr app: a buildable root, nothing configured (the Zephyr path row
/// starts open).
fn zephyr_app(tag: &str) -> (App, PathBuf) {
    let root = scratch(tag);
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.maybe_scan_devices();
    (app, root)
}

#[test]
fn ctrl_p_lands_on_the_first_open_question_and_toggles_back() {
    let (mut app, _root) = zephyr_app("toggle");
    app.place_startup_focus();
    let before = app.focus;
    assert_ne!(before, Focus::Project);

    app.handle(ctrl('p'));
    assert_eq!(app.focus, Focus::Project);
    assert_eq!(
        app.project_cursor, 0,
        "the Zephyr path question is the first open one"
    );

    // Enter answers the row under the cursor: the installation picker.
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::DirPicker { .. })));
    app.handle(key(KeyCode::Esc));

    // The second press is the way back out.
    app.handle(ctrl('p'));
    assert_eq!(app.focus, before);
}

#[test]
fn tab_leaves_the_project_pane_at_the_tours_first_stop() {
    let (mut app, _root) = zephyr_app("leave");
    app.handle(ctrl('p'));
    assert_eq!(app.focus, Focus::Project);

    app.handle(key(KeyCode::Tab));
    assert_eq!(
        app.focus,
        Focus::Workspace,
        "Tab re-enters the tour at its first stop"
    );

    app.handle(ctrl('p'));
    app.handle(key(KeyCode::BackTab));
    assert_eq!(
        app.focus,
        Focus::Logs,
        "BackTab leaves at the tour's last stop"
    );
}

#[test]
fn the_micropython_pane_renders_its_four_rows() {
    let (mut app, root) = mpy_app("render");
    std::fs::write(
        root.join("requirements.txt"),
        "adafruit-circuitpython-neopixel\n",
    )
    .unwrap();

    let frame = render(&mut app, 100, 32);
    for label in ["Projects base", "Project path", "Dependencies", "Script"] {
        assert!(frame.contains(label), "missing the {label} row:\n{frame}");
    }
    assert!(
        frame.contains("✓ requirements.txt") && frame.contains("✗ manifest.py"),
        "dependency files are reported individually:\n{frame}"
    );
    assert!(
        frame.contains("unknown"),
        "a script belief the probe/monitor never formed is honest about it:\n{frame}"
    );
    assert!(
        frame.contains("Project files:"),
        "the local pane carries the project-files title:\n{frame}"
    );
}

#[test]
fn the_micropython_projects_folder_is_read_from_the_user_config() {
    let (_app, root) = mpy_app("configured");
    let apps = root.join("home/mpy-apps");
    std::fs::create_dir_all(&apps).unwrap();
    chiptui::settings::save_mpy_projects(
        &chiptui::settings::user_config_path(&chiptui::settings::config_dir_in(&root.join("home"))),
        &apps,
    )
    .unwrap();

    // A fresh session resolves it; an invalid path reports itself instead.
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    assert_eq!(app.mpy_projects.as_deref(), Some(apps.as_path()));

    chiptui::settings::save_mpy_projects(
        &chiptui::settings::user_config_path(&chiptui::settings::config_dir_in(&root.join("home"))),
        Path::new("/nonexistent-chiptui-mpy-apps"),
    )
    .unwrap();
    let mut broken = App::new(&root);
    broken.set_serial_dir(root.join("dev"));
    broken.set_home_dir(root.join("home"));
    broken.bootstrap();
    broken.manager.set_override(Some(BackendKind::MicroPython));
    broken.maybe_scan_devices();
    assert!(broken.mpy_projects.is_none());
    assert!(
        broken
            .mpy_projects_invalid
            .as_deref()
            .is_some_and(|m| m.contains("[micropython] projects")),
        "the invalid answer names the key to fix"
    );
}

#[test]
fn choosing_a_micropython_projects_folder_saves_and_chains_to_the_picker() {
    let (mut app, root) = mpy_app("choose-base");
    let home = root.join("home");
    std::fs::create_dir_all(home.join("mpy-apps/blink")).unwrap();

    // ctrl+p lands on the first open question --- the projects folder ---
    // and Enter opens its picker, which starts on "use this directory".
    app.handle(ctrl('p'));
    app.handle(key(KeyCode::Enter));
    let Overlay::DirPicker { purpose, path, .. } = app.overlay.clone().unwrap() else {
        panic!("the projects-folder question opens the directory picker");
    };
    assert_eq!(purpose, chiptui::workspace::DirPurpose::MpyProjects);
    assert_eq!(path, home);

    // Descend into mpy-apps (the first subdirectory sorts first), where
    // the reflex Enter accepts it.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Enter));

    let saved = chiptui::settings::user_config_path(&chiptui::settings::config_dir_in(&home));
    let text = std::fs::read_to_string(&saved).unwrap();
    assert!(
        text.contains(&format!(
            "projects = \"{}\"",
            home.join("mpy-apps").display()
        )),
        "the pick is saved where resolution reads it:\n{text}"
    );
    assert_eq!(
        app.mpy_projects.as_deref(),
        Some(home.join("mpy-apps").as_path())
    );
    assert!(
        matches!(app.overlay, Some(Overlay::ProjectPicker { mpy: true, .. })),
        "accepting a folder chains straight to the project picker"
    );
}

#[test]
fn picking_a_micropython_project_reroots_the_local_pane() {
    let (mut app, root) = mpy_app("pick");
    let apps = root.join("home/mpy-apps");
    std::fs::create_dir_all(apps.join("blink/src")).unwrap();
    std::fs::write(apps.join("blink/main.py"), "print('blink')\n").unwrap();
    std::fs::write(apps.join("blink/requirements.txt"), "\n").unwrap();
    std::fs::create_dir_all(apps.join("other")).unwrap();
    app.mpy_projects = Some(apps.clone());

    // The folder is answered and no pick is pending a question mark, so
    // the cursor starts at the top; one Down reaches the project row,
    // whose Enter opens the picker over the folder's subdirectories.
    app.handle(ctrl('p'));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ProjectPicker { mpy: true, .. })
    ));

    // "blink" sorts first; Enter picks it.
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.mpy_root.as_deref(), Some(apps.join("blink").as_path()));
    let browser = app.browser.as_ref().unwrap();
    assert_eq!(
        browser.local_root,
        apps.join("blink"),
        "the local pane follows the pick"
    );
    assert_eq!(browser.local_path, apps.join("blink"));
    assert_eq!(app.header_project(), "blink", "the header names the pick");

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Project files: blink/"),
        "the local pane's title names the picked project:\n{frame}"
    );
    assert!(
        frame.contains("✓ requirements.txt"),
        "the dependencies report reads the picked root:\n{frame}"
    );
}
