//! The Project pane (row 1): the shortcuts overlay's way in (`ctrl+k`, then
//! the pane's `e` letter), the MicroPython
//! project questions (projects folder, project pick), the two reports that
//! complete the checklist (dependency coverage against the device's `/lib`,
//! with `Enter` installing the requirements through `mip`, and the boot-file
//! comparison against the device's root), and the re-rooting a pick performs
//! on the file browser's local side. The Zephyr rows' flows are covered by
//! `build_view.rs`; this file covers the pane-level grammar and the
//! MicroPython half.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use chiptui::app::{App, Focus, Overlay};
use chiptui::backend::BackendKind;
use chiptui::event::AppEvent;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{
    click, enter_project_pane, fake_curl, fake_mpremote, fake_mpremote_second_board, key, render,
};

/// A board holding the same two boot files the project does: `main.py`
/// byte-identical, `boot.py` the same length with different contents.
fn fake_mpremote_boot_board() -> String {
    format!(
        "{}/tests/fixtures/bin/mpremote-boot-board",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Drives the four ticks the requirements poll waits for.
///
/// `requirements.txt` is read off the tick rather than the draw path, so
/// an *external* edit --- which is what a test writing the file simulates
/// --- reaches the row on the same one-second cadence the local file
/// listings refresh on. The app's own writes refresh it immediately.
fn tick(app: &mut App) {
    for _ in 0..4 {
        app.handle(AppEvent::Tick);
    }
}

/// Whether the log carries `needle` --- the modal covers the log pane, so
/// a rendered frame cannot answer this while the manager is open.
fn logged(app: &App, needle: &str) -> bool {
    app.logs
        .visible(200)
        .any(|entry| entry.message.contains(needle))
}

fn log_text(app: &App) -> String {
    app.logs
        .visible(200)
        .map(|entry| entry.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drives the app until the browser has no device command in flight
/// (`files_view.rs`'s `settle_app`, local copy).
fn settle(app: &mut App) {
    common::settle_while(app, busy, "a background process");
}

/// Whether anything test-relevant is still running: a device command or
/// the package index fetch.
fn busy(app: &App) -> bool {
    app.browser
        .as_ref()
        .is_some_and(chiptui::browser::Browser::is_busy)
        || matches!(
            app.package_index,
            chiptui::app::packages::PackageIndex::Fetching { .. }
        )
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
/// seams pointed at the scratch tree. The browser exists *before*
/// `maybe_scan_devices` runs, pointed at the fake `mpremote` --- an absent
/// browser is what makes the scan spawn the machine's real `mpremote`, and
/// whatever board happens to be plugged in would then answer it (its
/// chip-identity query races the fake's listings).
fn mpy_app(tag: &str) -> (App, PathBuf) {
    let root = scratch(tag);
    std::fs::write(root.join("main.py"), "print('hi')\n").unwrap();
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    let mut browser = chiptui::browser::Browser::new(&root);
    browser.set_tool_path(fake_mpremote());
    app.browser = Some(browser);
    // Opening the package manager starts the index fetch, so every app here
    // needs the fake `curl`: the suite must never reach the network.
    app.set_package_curl_tool_path(fake_curl());
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

/// A MicroPython app wired to the fake board, device root and `/lib`
/// already listed, carrying the given `requirements.txt`.
fn connected_mpy_app(tag: &str, requirements: &str) -> (App, PathBuf) {
    let (mut app, root) = mpy_app(tag);
    std::fs::write(root.join("requirements.txt"), requirements).unwrap();
    tick(&mut app);
    let mut browser = app.browser.take().unwrap();
    browser.set_tool_path(fake_mpremote());
    browser.load_device(&mut app.processes, None, true);
    app.browser = Some(browser);
    settle(&mut app);
    (app, root)
}

#[test]
fn the_shortcut_letter_lands_on_the_first_open_question() {
    let (mut app, _root) = zephyr_app("enter");
    app.place_startup_focus();
    assert_ne!(app.focus, Focus::Project);

    enter_project_pane(&mut app);
    assert_eq!(app.focus, Focus::Project);
    assert_eq!(
        app.project_cursor, 0,
        "the Zephyr path question is the first open one"
    );

    // Enter answers the row under the cursor: the installation picker.
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::DirPicker { .. })));
    app.handle(key(KeyCode::Esc));

    // The letter pressed again is a no-op: focus is already there, and the
    // way out is the Tab tour, not a second press.
    enter_project_pane(&mut app);
    assert_eq!(app.focus, Focus::Project);
}

#[test]
fn tab_leaves_the_project_pane_at_the_tours_first_stop() {
    let (mut app, _root) = zephyr_app("leave");
    enter_project_pane(&mut app);
    assert_eq!(app.focus, Focus::Project);

    app.handle(key(KeyCode::Tab));
    assert_eq!(
        app.focus,
        Focus::Workspace,
        "Tab re-enters the tour at its first stop"
    );

    enter_project_pane(&mut app);
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
    std::fs::write(root.join("requirements.txt"), "urequests\n").unwrap();
    tick(&mut app);

    let frame = render(&mut app, 100, 32);
    for label in [
        "Projects base",
        "Project path",
        "Dependencies",
        "Boot files",
    ] {
        assert!(frame.contains(label), "missing the {label} row:\n{frame}");
    }
    assert!(
        frame.contains("Dependencies  requirements.txt"),
        "the dependency row names its file while no /lib answer exists:\n{frame}"
    );
    assert!(
        frame.contains("no device listing yet"),
        "a boot-files report with nothing to compare against is honest:\n{frame}"
    );
    assert!(
        frame.contains("Files:"),
        "the local pane carries the project-files title:\n{frame}"
    );
}

#[test]
fn the_dependencies_row_counts_coverage_against_the_device_lib() {
    // The fake board's /lib holds `simple.py` only.
    let (mut app, root) = connected_mpy_app("coverage", "simple\nurequests\n");
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("1/2 in /lib"),
        "one of two requirements installed:\n{frame}"
    );

    std::fs::write(root.join("requirements.txt"), "simple\n").unwrap();
    tick(&mut app);
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("1/1 in /lib"),
        "complete coverage after the file narrows:\n{frame}"
    );
}

#[test]
fn the_install_all_row_carries_every_specification_in_one_command() {
    let (mut app, _root) = connected_mpy_app("install", "urequests\nimaginary\n");
    enter_project_pane(&mut app);
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    // `Enter` on the row opens the manager --- it used to install outright,
    // so the one gesture did two unrelated things depending on state.
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::Packages)));

    // The whole-file install is a *row* rather than a key, because the
    // filter line owns every printable character.
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Install all declared"),
        "the whole-file install is offered as a row:\n{frame}"
    );
    app.handle(key(KeyCode::Enter));
    settle(&mut app);

    assert!(
        logged(&app, "urequests, imaginary installed"),
        "one mip command carried every specification:\n{}",
        log_text(&app)
    );
}

/// A MicroPython app connected to the fake board, carrying `requirements`
/// --- the manager flow's harness. The `curl` seam is already the fake's.
fn searchable_app(tag: &str, requirements: &str) -> (App, PathBuf) {
    connected_mpy_app(tag, requirements)
}

/// Opens the manager the way the Dependencies row does: `s` on the row,
/// cursor already parked there.
fn open_manager(app: &mut App) {
    enter_project_pane(app);
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Char('s')));
}

#[test]
fn enter_on_a_missing_requirements_file_creates_it_and_opens_the_search() {
    // No requirements.txt at all: the fake board connection exists, but the
    // project root only holds main.py.
    let (mut app, root) = connected_mpy_app("create", "will-be-absent\n");
    std::fs::remove_file(root.join("requirements.txt")).unwrap();
    app.set_package_curl_tool_path(fake_curl());

    enter_project_pane(&mut app);
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    settle(&mut app);

    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert!(
        text.contains("# MicroPython package requirements"),
        "the file starts from the template that documents itself:\n{text}"
    );
    assert!(
        !text.contains("will-be-absent"),
        "nothing is invented for the user:\n{text}"
    );
    assert!(
        matches!(app.overlay, Some(Overlay::Packages)),
        "the fresh file opens straight into the manager for its first package"
    );
}

#[test]
fn the_manager_merges_the_file_the_board_and_the_index_into_one_list() {
    // The fake board's /lib holds `simple.py`; the file declares only
    // `urequests`; the index offers three packages. All three sources reach
    // the same list, each row marked with where it stands.
    let (mut app, _root) = searchable_app("list", "urequests\n");
    open_manager(&mut app);
    settle(&mut app);

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("□ urequests"),
        "declared but not on the board:\n{frame}"
    );
    assert!(
        frame.contains("⚠ simple"),
        "on the board and the file does not know --- a state a search-only \
         window could not even show:\n{frame}"
    );
    for name in ["collections-deque", "umqtt.simple"] {
        assert!(
            frame.contains(name),
            "the catalogue offers {name}:\n{frame}"
        );
    }
    assert!(
        frame.contains("1 declared · 1 in /lib"),
        "the counts agree with the Dependencies row's own fraction:\n{frame}"
    );

    // Typing narrows every group at once.
    for c in "umqtt".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("umqtt.simple"),
        "the match survives:\n{frame}"
    );
    assert!(
        !frame.contains("collections-deque"),
        "the non-match is filtered out:\n{frame}"
    );
}

#[test]
fn a_letter_the_vim_keys_used_to_swallow_is_typable() {
    // `j` and `k` were bound to the cursor *before* the printable arm, so
    // `json` and `keyboard` could not be typed into the filter at all.
    let (mut app, _root) = searchable_app("typing", "urequests\n");
    open_manager(&mut app);
    settle(&mut app);
    for c in "jk".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("jk_"),
        "both letters reached the field:\n{frame}"
    );
}

#[test]
fn a_typed_spec_is_offered_for_installation_when_nothing_matches() {
    // The old search had no escape hatch here: a filter matching nothing
    // offered nothing, so a github: spec could not be installed at all.
    let (mut app, root) = searchable_app("manual", "urequests\n");
    open_manager(&mut app);
    settle(&mut app);
    for c in "github:org/repo".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("github:org/repo"),
        "the typed specification is offered as its own row:\n{frame}"
    );

    app.handle(key(KeyCode::Enter));
    settle(&mut app);
    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert!(
        text.lines().any(|line| line == "github:org/repo"),
        "and it lands in the file verbatim:\n{text}"
    );
}

#[test]
fn picking_a_catalogue_row_declares_it_and_installs_it() {
    let (mut app, root) = searchable_app("pick-row", "simple\n");
    open_manager(&mut app);
    settle(&mut app);

    // Filtering to the row is steadier than counting Downs: the list mixes
    // three sources and its order is by usefulness, not alphabet.
    for c in "urequests".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));
    settle(&mut app);

    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert!(
        text.lines().any(|line| line == "urequests"),
        "the pick lands as its own line:\n{text}"
    );
    assert!(
        logged(&app, "urequests added to"),
        "the append says where it went:\n{}",
        log_text(&app)
    );
    assert!(
        logged(&app, "urequests installed"),
        "the pick installs immediately:\n{}",
        log_text(&app)
    );

    // The window stays open for the next package; Esc closes it.
    assert!(matches!(app.overlay, Some(Overlay::Packages)));
    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
}

#[test]
fn a_package_is_declared_only_once_however_often_it_is_picked() {
    let (mut app, root) = searchable_app("dedupe", "simple\n");
    open_manager(&mut app);
    settle(&mut app);
    for c in "urequests".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));
    settle(&mut app);
    app.handle(key(KeyCode::Enter));
    settle(&mut app);

    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert_eq!(
        text.lines().filter(|line| *line == "urequests").count(),
        1,
        "a second pick must not append a duplicate line:\n{text}"
    );
}

#[test]
fn a_failed_index_fetch_names_itself_in_the_window() {
    let (mut app, _root) = connected_mpy_app("fetch-fail", "urequests\n");
    // The mpremote fake exits 1 on any invocation --- a curl that cannot
    // fetch, without touching the machine's network.
    app.set_package_curl_tool_path(fake_mpremote());

    open_manager(&mut app);
    settle(&mut app);

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("could not fetch the package index"),
        "the failure is a named state, not a silent empty list:\n{frame}"
    );
}

#[test]
fn the_boot_files_row_compares_the_project_against_the_device() {
    // The main fake board's root has neither boot.py nor main.py, so the
    // project's own main.py is the row's one honest fact.
    let (mut app, _root) = connected_mpy_app("boot", "simple\n");
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Boot files"),
        "the row renders with a device listing:\n{frame}"
    );
    assert!(
        frame.contains("main.py →"),
        "a main.py that was never uploaded says so:\n{frame}"
    );
    assert!(
        !frame.contains("boot.py"),
        "a boot.py neither side has costs no display:\n{frame}"
    );

    // The second board has a boot.py of its own and no main.py: both
    // directions of the comparison show.
    drop(app);
    let (mut app, root) = mpy_app("boot2");
    std::fs::write(root.join("requirements.txt"), "urequests\n").unwrap();
    tick(&mut app);
    let mut browser = app.browser.take().unwrap();
    browser.set_tool_path(fake_mpremote_second_board());
    browser.load_device(&mut app.processes, None, true);
    app.browser = Some(browser);
    settle(&mut app);
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("boot.py ← · main.py →"),
        "device-only boot and local-only main both report:\n{frame}"
    );
    assert!(
        frame.contains("0/1 in /lib"),
        "a board without /lib reads as zero installed, not as an error:\n{frame}"
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

    // The `e` letter lands on the first open question --- the projects
    // folder --- and Enter opens its picker, which starts on "use this directory".
    enter_project_pane(&mut app);
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
    enter_project_pane(&mut app);
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
        frame.contains("Files: blink/"),
        "the local pane's title names the picked project:\n{frame}"
    );
    assert!(
        frame.contains("Dependencies  requirements.txt"),
        "the dependencies report reads the picked root:\n{frame}"
    );
}

/// A project laid out the way the MicroPython scaffold lays it out: the two
/// entry points live in `src/`, not at the root.
fn scaffolded_mpy_app(tag: &str, requirements: &str) -> (App, PathBuf) {
    let (mut app, root) = mpy_app(tag);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/boot.py"), "# boot\n").unwrap();
    std::fs::write(root.join("src/main.py"), "print('hi')\n").unwrap();
    // Written before the connection: the `/lib` listing is what arms the
    // sub-listing a dotted name needs, and it reads this file.
    std::fs::write(root.join("requirements.txt"), requirements).unwrap();
    tick(&mut app);
    let mut browser = app.browser.take().unwrap();
    browser.set_tool_path(fake_mpremote_boot_board());
    browser.load_device(&mut app.processes, None, true);
    app.browser = Some(browser);
    settle(&mut app);
    (app, root)
}

#[test]
fn the_boot_row_reads_the_projects_src_directory_not_its_root() {
    // The scaffold writes `src/boot.py` and `src/main.py`, and the Files
    // pane opens on `src/`. The row used to compare the device's root
    // against the *project* root, where those files never are, so every
    // scaffolded project reported `boot.py ←` (device-only) forever.
    let (mut app, _root) = scaffolded_mpy_app("boot-src", "urequests\n");
    let frame = render(&mut app, 100, 32);
    assert!(
        !frame.contains("boot.py ←") && !frame.contains("main.py ←"),
        "the project's own copies are found, so neither file is device-only:\n{frame}"
    );
}

#[test]
fn the_boot_row_verifies_contents_in_the_background_rather_than_resting_on_size() {
    // Both files match the device's by size, so a size comparison alone
    // can only say `≈` --- which the row reported as a warning, for the
    // whole life of a perfectly synchronised project. The root listing now
    // arms a silent sha256 of each, and the verdicts are real.
    let (mut app, _root) = scaffolded_mpy_app("boot-hash", "urequests\n");
    settle(&mut app);
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("main.py ="),
        "an identical main.py is verified, not merely the same size:\n{frame}"
    );
    assert!(
        frame.contains("boot.py ≠"),
        "same length, different contents --- exactly what a size check misses:\n{frame}"
    );
    // And the summary mark reports the worst of the two: a differing
    // boot.py must not hide behind main.py's green check.
    assert!(
        frame.contains("✗ Boot files"),
        "the row summarises the worse of the two files:\n{frame}"
    );
}

#[test]
fn the_background_hashes_are_silent() {
    // The row is the consumer, not the log --- the same silence the `/lib`
    // coverage listing keeps. The user's own `c` in the Files pane is what
    // announces a comparison.
    let (mut app, _root) = scaffolded_mpy_app("boot-quiet", "urequests\n");
    settle(&mut app);
    let frame = render(&mut app, 100, 32);
    assert!(
        !frame.contains("contents identical") && !frame.contains("contents differ"),
        "a background verification says nothing in the log:\n{frame}"
    );
}

#[test]
fn a_dotted_requirement_is_found_in_its_own_package_directory() {
    // `umqtt.simple` lands in `/lib/umqtt/simple.mpy`: matched flat against
    // `/lib`, it read as missing forever and pinned the row at ⚠.
    let (mut app, _root) = scaffolded_mpy_app("dotted", "umqtt.simple\nurequests\n");
    settle(&mut app);
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("2/2 in /lib"),
        "both requirements resolve, the dotted one under its own directory:\n{frame}"
    );
}

#[test]
fn del_removes_a_package_from_the_file_and_the_board() {
    // The board carries `/lib/urequests.mpy` and the file declares it, so
    // both halves of the removal apply. Removal did not exist at all before:
    // the window could only add.
    let (mut app, root) = scaffolded_mpy_app("remove", "# keep me\nurequests\n");
    settle(&mut app);
    open_manager(&mut app);
    settle(&mut app);
    for c in "urequests".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Delete));

    // The destructive grammar of `SPEC.md` §15: the action as a question,
    // what it happens to, what is lost, then the literal command.
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Remove this package?"),
        "the title asks the action:\n{frame}"
    );
    assert!(
        frame.contains("/lib/urequests.mpy"),
        "the target names what is deleted:\n{frame}"
    );
    assert!(
        frame.contains("requirements.txt"),
        "and the consequence names the other half:\n{frame}"
    );
    assert!(
        frame.contains("mpremote") && frame.contains("rm"),
        "the literal command rides underneath:\n{frame}"
    );

    // No is the default, as in every other destructive dialog.
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::Packages)),
        "declining hands the manager back --- the overlay slot is one deep"
    );
    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert!(text.lines().any(|line| line == "urequests"), "nothing lost");

    // Accepting acts on both.
    app.handle(key(KeyCode::Delete));
    app.handle(key(KeyCode::Char('y')));
    settle(&mut app);

    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert!(
        !text.lines().any(|line| line == "urequests"),
        "the declaration is gone:\n{text}"
    );
    assert!(
        text.contains("# keep me"),
        "and the file's own comments survive:\n{text}"
    );
    assert!(
        logged(&app, "/lib/urequests.mpy removed"),
        "the board's copy is deleted too:\n{}",
        log_text(&app)
    );
    assert!(
        matches!(app.overlay, Some(Overlay::Packages)),
        "the window comes back for the next removal"
    );
}

#[test]
fn enter_on_an_installed_package_asks_to_remove_it_instead_of_reinstalling() {
    // The board carries `/lib/urequests.mpy` and the file declares it (the
    // `✓` mark), so `Enter` is no longer a way to reinstall it --- it opens
    // the same removal confirmation `Del` does, since re-running an install
    // mip would just skip is not worth the key, and uninstalling is the one
    // thing an already-installed row could not reach before.
    let (mut app, root) = scaffolded_mpy_app("reinstall-asks", "urequests\n");
    settle(&mut app);
    open_manager(&mut app);
    settle(&mut app);
    for c in "urequests".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));

    let frame = render(&mut app, 100, 32);
    assert!(
        matches!(app.overlay, Some(Overlay::ConfirmRemovePackage { .. })),
        "enter on an installed row must ask to remove it:\n{frame}"
    );
    assert!(
        frame.contains("Remove this package?"),
        "the same destructive question del opens:\n{frame}"
    );

    // Declining leaves everything untouched --- no reinstall happened either.
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::Packages)));
    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert!(text.lines().any(|line| line == "urequests"), "nothing lost");
    assert!(
        !logged(&app, "urequests installed"),
        "enter must not have reinstalled it under the confirmation:\n{}",
        log_text(&app)
    );
}

#[test]
fn installing_from_the_manager_asks_before_interrupting_a_busy_device() {
    // A regression test: installing from inside the manager used to be a
    // silent no-op on a busy device, because `check_interrupt_gate` refused
    // to open on top of any overlay --- including `Overlay::Packages` itself.
    let (mut app, root) = searchable_app("busy-install", "simple\n");
    open_manager(&mut app);
    settle(&mut app);

    // Simulate the belief a real probe would have formed: the browser holds
    // every device request behind the interrupt gate.
    let mut browser = app.browser.take().unwrap();
    browser.set_interrupt_gate(true, &mut app.processes, None);
    app.browser = Some(browser);

    for c in "urequests".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));

    assert!(
        matches!(
            app.overlay,
            Some(Overlay::ConfirmInterruptDevice {
                return_to_packages: true,
                ..
            })
        ),
        "a gated install must ask instead of vanishing:\n{:?}",
        app.overlay
    );

    // Accepting hands the manager back immediately, not the bare dashboard.
    app.handle(key(KeyCode::Char('y')));
    assert!(
        matches!(app.overlay, Some(Overlay::Packages)),
        "accepting the interrupt returns to the manager:\n{:?}",
        app.overlay
    );

    settle(&mut app);
    // The restore question may follow once the install drains; its default
    // ("leave it stopped") is exercised elsewhere, so just clear it here.
    if matches!(app.overlay, Some(Overlay::RestoreDeviceScript { .. })) {
        app.handle(key(KeyCode::Enter));
    }

    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert!(
        text.lines().any(|line| line == "urequests"),
        "the install still completes once the user unblocks it:\n{text}"
    );
    assert!(
        logged(&app, "urequests installed"),
        "the mip command actually ran:\n{}",
        log_text(&app)
    );
    assert!(
        matches!(app.overlay, Some(Overlay::Packages)),
        "and the manager is what's left showing"
    );
}

#[test]
fn removing_a_package_asks_before_interrupting_a_busy_device() {
    // The sibling of the install-side bug: `ConfirmRemovePackage`'s accept
    // closure used to overwrite `self.overlay` with `Packages` unconditionally
    // after calling `remove_package`, clobbering a `ConfirmInterruptDevice`
    // that call had just opened.
    let (mut app, _root) = scaffolded_mpy_app("remove-busy", "urequests\n");
    settle(&mut app);
    open_manager(&mut app);
    settle(&mut app);
    for c in "urequests".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Delete));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmRemovePackage { .. })
    ));

    let mut browser = app.browser.take().unwrap();
    browser.set_interrupt_gate(true, &mut app.processes, None);
    app.browser = Some(browser);

    app.handle(key(KeyCode::Char('y')));

    assert!(
        matches!(
            app.overlay,
            Some(Overlay::ConfirmInterruptDevice {
                return_to_packages: true,
                ..
            })
        ),
        "a gated removal must ask, not get clobbered back to the manager silently:\n{:?}",
        app.overlay
    );
}

#[test]
fn removing_a_dotted_package_deletes_its_leaf_not_the_shared_directory() {
    // `umqtt.simple` lives at `/lib/umqtt/simple.mpy`. Deleting the
    // `umqtt/` directory would take any sibling package with it.
    let (mut app, _root) = scaffolded_mpy_app("remove-dotted", "umqtt.simple\n");
    settle(&mut app);
    open_manager(&mut app);
    settle(&mut app);
    for c in "umqtt".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Delete));

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("/lib/umqtt/simple.mpy"),
        "the leaf is the target:\n{frame}"
    );
    assert!(
        !frame.contains("rm :/lib/umqtt\n") && !frame.contains("recursive"),
        "the shared directory is not swept away:\n{frame}"
    );
}

#[test]
fn a_click_in_the_manager_selects_without_installing() {
    // Picker grammar: a click moves the cursor, and only the keyboard's
    // `Enter` acts. Getting this wrong would install a package on a stray
    // click, which is the reason the rule exists.
    let (mut app, root) = searchable_app("click", "urequests\n");
    app.set_mouse_enabled(true);
    open_manager(&mut app);
    settle(&mut app);

    let frame = render(&mut app, 100, 32);
    let (row, column) = find_cell(&frame, "umqtt.simple").expect("the row is drawn");
    app.handle(AppEvent::Mouse(click(column, row)));

    assert_eq!(
        app.packages_state().selected,
        app.package_rows()
            .iter()
            .position(|r| matches!(r, chiptui::app::packages::RowKind::Package(p)
                if p.name == "umqtt.simple"))
            .expect("the row is in the model"),
        "the click landed on the row it was aimed at"
    );
    let text = std::fs::read_to_string(root.join("requirements.txt")).unwrap();
    assert!(
        !text.contains("umqtt.simple"),
        "and it did not install anything:\n{text}"
    );

    // Clicking the details pane hands it the keyboard, the docs pickers'
    // own rule.
    let (row, column) = find_cell(&frame, "Details").expect("the pane is drawn");
    app.handle(AppEvent::Mouse(click(column, row + 2)));
    assert_eq!(app.packages_state().focus, chiptui::app::DocsFocus::Details);
}

/// The drawn row and column of `needle`'s first cell. Byte offsets are not
/// columns --- the frame is full of multi-byte borders --- so the search is
/// per rendered line.
fn find_cell(frame: &str, needle: &str) -> Option<(u16, u16)> {
    for (row, line) in frame.lines().enumerate() {
        if let Some(byte) = line.find(needle) {
            let column = line[..byte].chars().count() as u16;
            return Some((row as u16, column));
        }
    }
    None
}
