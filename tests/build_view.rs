//! The build panel end to end, against the fake `west`: the panel appears
//! for a build-capable backend without a filesystem, `Enter` runs the
//! composed command and streams into the Monitor tab, `Clean` asks first,
//! and `Stop` cancels a long-running command.

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::app::{App, Focus, LogTab, MonitorSource, Overlay, View};
use chiptui::backend::BackendKind;
use chiptui::backend::BuildKind;
use chiptui::event::AppEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn fake(tool: &str) -> String {
    format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A Zephyr app in a temp directory: a real project layout so the panel has
/// a root, with a CMakeCache claiming a board when the test wants one.
fn zephyr_app(tag: &str, board: Option<&str>) -> (App, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("chiptui-buildview-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    if let Some(board) = board {
        std::fs::create_dir_all(root.join("build/zephyr")).unwrap();
        std::fs::write(
            root.join("build/zephyr/CMakeCache.txt"),
            format!("CACHED_BOARD:STRING={board}\n"),
        )
        .unwrap();
    }

    // The serial scan must not look at the machine's real /dev, and workspace
    // discovery must not look at the machine's real $HOME (a ~/zephyrproject
    // on the host would resolve the pane differently): both point at fixture
    // directories so startup is deterministic. Set before `bootstrap`, whose
    // tool report already resolves the workspace.
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.maybe_scan_devices();
    // The binary's startup sequence, mirrored: focus lands on the first
    // pane row 2 actually shows.
    app.place_startup_focus();
    (app, root)
}

/// An app whose build panel runs the fake `west`.
fn app_with_west(tag: &str, tool: &str) -> App {
    let (mut app, _root) = zephyr_app(tag, Some("nrf52840dk/nrf52840"));
    app.build.as_mut().unwrap().set_tool_path(fake(tool));
    app
}

/// Drains process events into the app until `done` holds or time runs out.
fn pump_until(app: &mut App, mut done: impl FnMut(&App) -> bool, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        if done(app) {
            return true;
        }
        app.handle(AppEvent::Tick);
        std::thread::sleep(Duration::from_millis(5));
    }
    done(app)
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| chiptui::ui::draw(frame, app))
        .unwrap();
    terminal.backend().to_string()
}

#[test]
fn the_panel_appears_and_is_a_focus_stop_for_a_build_backend() {
    let (mut app, _root) = zephyr_app("focus", Some("nrf52840dk/nrf52840"));

    assert!(app.build.is_some(), "Zephyr gets a build panel");
    assert!(app.build_pane_visible());
    assert_eq!(
        app.focus,
        Focus::Workspace,
        "startup focus lands on the workspace pane, not the paneless FilesLocal default"
    );

    // Tab tour: Workspace -> Build -> Logs -> Workspace.
    app.focus = Focus::Logs;
    app.handle(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Workspace);
    app.handle(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Build);
    app.handle(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Logs);

    // And it renders: the checklist with its answers, the buttons below.
    app.focus = Focus::Build;
    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("Build"), "missing panel:\n{frame}");
    assert!(
        frame.contains("nrf52840dk/nrf52840"),
        "cached board not shown:\n{frame}"
    );
    assert!(
        frame.contains("× Clean"),
        "the lifecycle buttons must show:\n{frame}"
    );
    assert!(
        frame.contains("Board"),
        "the board checklist row must show:\n{frame}"
    );
}

#[test]
fn a_backend_without_build_never_gets_the_panel() {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    assert!(app.build.is_none(), "MicroPython builds nothing");
    assert!(!app.build_pane_visible());
}

#[test]
fn enter_builds_and_streams_into_the_monitor_tab() {
    let mut app = app_with_west("run", "west");
    app.focus = Focus::Build;

    // The list is Menuconfig, Clean, Build, Rebuild, Flash: two rows down
    // sits Build.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));

    assert!(app.build.as_ref().unwrap().is_busy());
    assert_eq!(app.view, View::Dashboard);
    // The Monitor tab is *shown*, but focus stays on the panel: the
    // lifecycle's next step (Stop, then Flash) is right here.
    assert_eq!(app.focus, Focus::Build);
    assert_eq!(app.log_tab, LogTab::Monitor);
    assert_eq!(app.monitor_source, MonitorSource::Build);
    let caps = app.manager.capabilities();
    assert_eq!(
        app.build
            .as_ref()
            .unwrap()
            .action_at(&caps, app.build.as_ref().unwrap().cursor),
        Some(chiptui::build::BuildAction::Stop),
        "a running build parks the cursor on Stop"
    );

    // The build directory exists in the fixture, so the command is the
    // incremental `west build` --- no `-b`, the cache already carries it.
    // (The tool path leads the line: the override pointing at the fake.)
    assert!(
        app.build
            .as_ref()
            .unwrap()
            .output
            .front()
            .unwrap()
            .ends_with("west build")
    );

    let finished = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().last.is_some(),
        10,
    );
    assert!(finished, "the fake west never finished");
    let last = app.build.as_ref().unwrap().last.as_ref().unwrap();
    assert!(last.ok, "a zero-exit west must report success");
    assert_eq!(last.what, "Build");
    let caps = app.manager.capabilities();
    assert_eq!(
        app.build
            .as_ref()
            .unwrap()
            .action_at(&caps, app.build.as_ref().unwrap().cursor),
        Some(chiptui::build::BuildAction::Flash),
        "a successful build moves the cursor to Flash"
    );
    assert!(
        app.build.as_ref().unwrap().output.len() >= 2,
        "output streamed"
    );

    // The Monitor tab shows the streamed output, tail-following.
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("fake west: build"),
        "output missing:\n{frame}"
    );
}

#[test]
fn clean_asks_before_running() {
    let mut app = app_with_west("clean", "west");
    app.focus = Focus::Build;

    app.handle(key(KeyCode::Down)); // Build
    app.handle(key(KeyCode::Enter)); // Clean

    // Destructive capability (Capability::Clean): a confirm quoting the
    // literal command, defaulting to No.
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmBuild {
            action: chiptui::build::BuildAction::Build(BuildKind::Clean),
            confirm: false
        })
    );
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("west build -t clean"),
        "the confirm must quote the literal command:\n{frame}"
    );

    // Accept with 'y': the command runs, and the cursor waits it out on
    // Build --- the step a clean exists to clear the way for.
    app.handle(key(KeyCode::Char('y')));
    assert!(app.build.as_ref().unwrap().is_busy());
    let caps = app.manager.capabilities();
    assert_eq!(
        app.build
            .as_ref()
            .unwrap()
            .action_at(&caps, app.build.as_ref().unwrap().cursor),
        Some(chiptui::build::BuildAction::Build(BuildKind::Build)),
        "a running clean parks the cursor on Build"
    );
    assert!(
        app.build
            .as_ref()
            .unwrap()
            .output
            .front()
            .unwrap()
            .ends_with("west build -t clean")
    );

    // Declining leaves nothing running.
    let mut app2 = app_with_west("clean-decline", "west");
    app2.focus = Focus::Build;
    app2.handle(key(KeyCode::Down)); // Clean
    app2.handle(key(KeyCode::Enter));
    app2.handle(key(KeyCode::Esc));
    assert!(!app2.build.as_ref().unwrap().is_busy());
}

#[test]
fn rebuild_is_pristine_and_pins_the_cached_board() {
    let mut app = app_with_west("rebuild", "west");
    app.focus = Focus::Build;

    for _ in 0..3 {
        // Menuconfig, Clean, Build
        app.handle(key(KeyCode::Down));
    } // Rebuild
    app.handle(key(KeyCode::Enter));

    assert!(
        app.build.as_ref().unwrap().is_busy(),
        "rebuild is not destructive: no confirm, straight to running"
    );
    assert!(
        app.build
            .as_ref()
            .unwrap()
            .output
            .front()
            .unwrap()
            .ends_with("west build --pristine=always -b nrf52840dk/nrf52840")
    );
    let _ = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().last.is_some(),
        10,
    );
}

#[test]
fn stop_cancels_the_running_command() {
    // `slow` ignores arguments, prints one line and blocks: a build command
    // that outlives the user's patience.
    let mut app = app_with_west("stop", "slow");
    app.focus = Focus::Build;

    // Menuconfig, Clean, then Build.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    assert!(app.build.as_ref().unwrap().is_busy());

    // While running, Stop heads the list and holds the cursor (starting
    // the build left focus on the panel): Enter cancels.
    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("Stop"), "no stop row:\n{frame}");
    assert_eq!(app.focus, Focus::Build);
    app.handle(key(KeyCode::Enter));

    let cancelled = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().last.is_some(),
        10,
    );
    assert!(cancelled, "the cancellation never reported");
    let last = app.build.as_ref().unwrap().last.as_ref().unwrap();
    assert!(!last.ok, "a cancelled build is not a success");
}

#[test]
fn a_failed_command_reports_and_keeps_the_panel_usable() {
    // `noisy` writes to stderr and exits non-zero (see fixtures).
    let mut app = app_with_west("fail", "noisy");
    app.focus = Focus::Build;

    // Menuconfig, Clean, then Build.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    let finished = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().last.is_some(),
        10,
    );
    assert!(finished);
    let last = app.build.as_ref().unwrap().last.as_ref().unwrap();
    assert!(!last.ok, "a non-zero exit must be a failure");

    // The panel is idle again, the cursor fell back on Build (the retry),
    // and that retry is one Enter away.
    assert!(!app.build.as_ref().unwrap().is_busy());
    let caps = app.manager.capabilities();
    assert_eq!(
        app.build
            .as_ref()
            .unwrap()
            .action_at(&caps, app.build.as_ref().unwrap().cursor),
        Some(chiptui::build::BuildAction::Build(BuildKind::Build)),
        "a failed build moves the cursor back to Build"
    );
    app.focus = Focus::Build;
    app.handle(key(KeyCode::Enter));
    assert!(app.build.as_ref().unwrap().is_busy());
}

#[test]
fn switching_to_micropython_hides_the_panel_and_reclamps_focus() {
    let (mut app, _root) = zephyr_app("switch", None);
    app.focus = Focus::Build;

    // The real path: the picker applies the override and re-clamps.
    app.handle(key(KeyCode::Char('o')));
    app.handle(key(KeyCode::Up)); // Zephyr -> MicroPython (wrapping past Automatic)
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.manager.selected_kind(), Some(BackendKind::MicroPython));
    assert!(!app.build_pane_visible());
    assert_eq!(
        app.focus,
        Focus::FilesLocal,
        "focus must fall back to the local pane, not a pane that is gone"
    );
}

#[test]
fn the_board_picker_fetches_filters_and_picks_for_the_session() {
    // No CMakeCache: nothing is known until the user picks.
    let (mut app, root) = zephyr_app("picker", None);
    app.build.as_mut().unwrap().set_tool_path(fake("west"));
    app.focus = Focus::Build;

    // The Board checklist row lives in the workspace pane now, below the
    // other three questions.
    app.focus = Focus::Workspace;
    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::BoardPicker { .. })));

    // First open kicked off the background `west boards` fetch.
    assert!(matches!(
        app.build.as_ref().unwrap().boards.state,
        chiptui::build::ListState::Loading
    ));
    let loaded = pump_until(
        &mut app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().boards.state,
                chiptui::build::ListState::Loaded(_)
            )
        },
        10,
    );
    assert!(loaded, "the fake west boards never finished");

    // The modal lists the targets.
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("native/native64"),
        "list not shown:\n{frame}"
    );
    assert!(
        frame.contains("west boards"),
        "the modal must name its source:\n{frame}"
    );

    // Typing filters (case-insensitively, name or description)…
    app.handle(key(KeyCode::Char('N')));
    app.handle(key(KeyCode::Char('R')));
    app.handle(key(KeyCode::Char('F')));
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("nrf52840dk/nrf52840"),
        "filter failed:\n{frame}"
    );
    assert!(
        !frame.contains("native/native64"),
        "a non-matching target must be filtered out:\n{frame}"
    );

    // …and Enter picks for this session: commands carry -b, nothing is
    // written to the project.
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.build.as_ref().unwrap().board_name(),
        Some("nrf52840dk/nrf52840")
    );
    assert_eq!(
        app.build.as_ref().unwrap().board.as_ref().unwrap().origin,
        chiptui::build::BoardOrigin::Picked
    );
    assert!(
        !root.join("build/zephyr/CMakeCache.txt").exists(),
        "a pick must not write project configuration"
    );

    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("picked"),
        "the header must say the board's origin:\n{frame}"
    );
    let backend = app.manager.backend().unwrap();
    let build = app
        .build
        .as_ref()
        .unwrap()
        .command(chiptui::backend::BuildKind::Build, backend)
        .unwrap();
    assert!(
        build.to_string().ends_with("-b nrf52840dk/nrf52840"),
        "the picked board must reach the first build: {}",
        build
    );
}

#[test]
fn the_shield_picker_lists_picks_and_clears_for_the_session() {
    // No CMakeCache and no build directory: the first build carries every
    // configuration flag the answers produce.
    let (mut app, root) = zephyr_app("shield", None);
    app.build.as_mut().unwrap().set_tool_path(fake("west"));
    app.focus = Focus::Workspace;

    // The Shield checklist row sits right under the Board one: four downs
    // from the cursor's start on Zephyr Base.
    for _ in 0..4 {
        app.handle(key(KeyCode::Down));
    }
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("Shield"),
        "the shield checklist row must show:\n{frame}"
    );
    assert!(
        frame.contains("none (optional)"),
        "an unset shield states itself as none, not as an open question:\n{frame}"
    );
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::ShieldPicker { .. })));

    // First open kicked off the background `west shields` fetch.
    assert!(matches!(
        app.build.as_ref().unwrap().shields.state,
        chiptui::build::ListState::Loading
    ));
    let loaded = pump_until(
        &mut app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().shields.state,
                chiptui::build::ListState::Loaded(_)
            )
        },
        10,
    );
    assert!(loaded, "the fake west shields never finished");

    // The modal lists the shields, with the (none) row to clear one.
    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("nrf7002ek"), "list not shown:\n{frame}");
    assert!(
        frame.contains("(none)"),
        "the clear row must show:\n{frame}"
    );

    // Typing filters; the first Down steps past the (none) row onto the
    // first match, and Enter picks for this session.
    for c in ['n', 'r', 'f', '7'] {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.build.as_ref().unwrap().shield_name(),
        Some("nrf7002ek"),
        "the pick must land"
    );
    assert!(
        !root.join("build/zephyr/CMakeCache.txt").exists(),
        "a shield pick must not write project configuration"
    );

    // The pick reaches the first build as --shield.
    let backend = app.manager.backend().unwrap();
    let build = app
        .build
        .as_ref()
        .unwrap()
        .command(chiptui::backend::BuildKind::Build, backend)
        .unwrap();
    assert!(
        build.to_string().ends_with("west build --shield nrf7002ek"),
        "the picked shield must reach the first build: {build}"
    );

    // The checklist row shows the answer, and the (none) row clears it.
    // (The workspace cursor never moved: it still sits on the Shield row.)
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("nrf7002ek"),
        "the answer must show:\n{frame}"
    );
    app.focus = Focus::Workspace;
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.build.as_ref().unwrap().shield_name(),
        None,
        "Enter on the (none) row clears the shield"
    );
    let build = app
        .build
        .as_ref()
        .unwrap()
        .command(
            chiptui::backend::BuildKind::Build,
            app.manager.backend().unwrap(),
        )
        .unwrap();
    assert!(
        build.to_string().ends_with("west build"),
        "no shield is no flag at all: {build}"
    );
}

#[test]
fn a_boardless_filter_match_enter_picks_nothing_and_esc_changes_nothing() {
    let (mut app, _root) = zephyr_app("picker-esc", Some("nrf52840dk/nrf52840"));
    app.build.as_mut().unwrap().set_tool_path(fake("west"));
    app.focus = Focus::Workspace;

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    } // the Board checklist row
    app.handle(key(KeyCode::Enter));
    assert!(pump_until(
        &mut app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().boards.state,
                chiptui::build::ListState::Loaded(_)
            )
        },
        10
    ));

    // A filter that matches nothing: Enter falls through without a pick…
    app.handle(key(KeyCode::Char('z')));
    app.handle(key(KeyCode::Char('z')));
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.build.as_ref().unwrap().board_name(),
        Some("nrf52840dk/nrf52840"),
        "an impossible selection must not change the board"
    );
    assert_eq!(
        app.build.as_ref().unwrap().board.as_ref().unwrap().origin,
        chiptui::build::BoardOrigin::Cache
    );

    // …and Esc leaves the cache answer untouched either way.
    app.focus = Focus::Workspace;
    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Esc));
    assert_eq!(
        app.build.as_ref().unwrap().board_name(),
        Some("nrf52840dk/nrf52840")
    );
}

#[test]
fn a_missing_west_explains_itself_in_the_picker() {
    let (mut app, _root) = zephyr_app("picker-missing", None);
    // A tool override pointing at a path that does not exist: the fetch
    // fails to spawn, and the picker must say so instead of hanging.
    app.build
        .as_mut()
        .unwrap()
        .set_tool_path("/nonexistent/west");
    app.focus = Focus::Workspace;

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    } // the Board checklist row
    app.handle(key(KeyCode::Enter));

    let failed = pump_until(
        &mut app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().boards.state,
                chiptui::build::ListState::Failed(_)
            )
        },
        10,
    );
    assert!(failed, "the failed spawn never reported");
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("is west on PATH?"),
        "the picker must explain the failure:\n{frame}"
    );
}

#[test]
fn flash_is_listed_confirms_and_runs_through_west() {
    let mut app = app_with_west("flash", "west");
    app.focus = Focus::Build;

    // Flash sits last: Menuconfig, Clean, Build, Rebuild, then it.
    for _ in 0..4 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));

    // Destructive (Capability::Flash): the confirm quotes the literal
    // command, defaulting to No.
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmBuild {
            action: chiptui::build::BuildAction::Flash,
            confirm: false
        })
    );
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("west flash"),
        "the confirm must quote the literal command:\n{frame}"
    );

    // Declining runs nothing.
    app.handle(key(KeyCode::Esc));
    assert!(!app.build.as_ref().unwrap().is_busy());

    // Accepting runs it, streaming into the Monitor tab, and the report
    // line names Flash --- not a recycled Build label.
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Char('y')));
    assert!(app.build.as_ref().unwrap().is_busy());
    assert_eq!(app.monitor_source, MonitorSource::Build);
    assert!(
        app.build
            .as_ref()
            .unwrap()
            .output
            .front()
            .unwrap()
            .ends_with("west flash")
    );

    let finished = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().last.is_some(),
        10,
    );
    assert!(finished);
    let last = app.build.as_ref().unwrap().last.as_ref().unwrap();
    assert!(last.ok);
    assert_eq!(last.what, "Flash");
}

#[test]
fn x_routes_a_build_backend_to_west_flash_and_micropython_to_esptool() {
    // Zephyr: `x` opens the flash confirm of the build panel, not esptool's
    // dialog --- that dialog cannot talk to this board.
    let mut app = app_with_west("x-zephyr", "west");
    app.handle(key(KeyCode::Char('x')));
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmBuild {
            action: chiptui::build::BuildAction::Flash,
            confirm: false
        })
    );
    assert_ne!(app.view, View::Flash);

    // MicroPython: `x` still opens the esptool flash dialog.
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.handle(key(KeyCode::Char('x')));
    assert_eq!(app.view, View::Flash);
}

/// A west workspace under the fixture home: `.west/`, `zephyr/VERSION`, and
/// a venv `west` that is a plain copy of the fake (so the pane's status and
/// commands can assert on a real absolute path).
fn workspace_under(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    let dir = home.join(name);
    std::fs::create_dir_all(dir.join(".west")).unwrap();
    std::fs::create_dir_all(dir.join("zephyr")).unwrap();
    std::fs::write(dir.join(".west/config"), "[manifest]\npath = zephyr\n").unwrap();
    std::fs::write(
        dir.join("zephyr/VERSION"),
        "VERSION_MAJOR = 4\nVERSION_MINOR = 1\nPATCHLEVEL = 0\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
    std::fs::copy(fake("west"), dir.join(".venv/bin/west")).unwrap();
    dir
}

#[test]
fn the_workspace_pane_resolves_from_project_config_and_runs_update() {
    let (mut app, root) = zephyr_app("ws", None);
    let home = root.join("home");
    let ws = workspace_under(&home, "zephyrproject");
    // A toolchain outside CMake's default locations, pinned through the
    // same section: everything the environment needs comes from the file.
    let sdk = home.join("opt/zephyr-sdk-0.17.1");
    std::fs::create_dir_all(&sdk).unwrap();
    std::fs::write(sdk.join("sdk_version"), "0.17.1\n").unwrap();
    std::fs::write(
        root.join("chiptui.toml"),
        format!(
            "[zephyr]\nworkspace = \"{}\"\nsdk = \"{}\"\n",
            ws.display(),
            sdk.display()
        ),
    )
    .unwrap();
    // Re-run resolution now that the config exists.
    app.workspace = None;
    app.build = None;
    app.maybe_scan_devices();

    let panel = app.workspace.as_ref().unwrap();
    assert_eq!(panel.dir(), Some(&ws), "the explicit config resolves");
    assert_eq!(
        panel.resolved.as_ref().unwrap().sdk.as_deref(),
        Some(sdk.as_path()),
        "the toolchain location comes from the same section"
    );
    // The venv's west is what every command runs, and the environment says
    // which workspace it belongs to.
    assert!(
        app.build
            .as_ref()
            .unwrap()
            .tool_path()
            .unwrap()
            .starts_with(ws.join(".venv/bin/west").to_str().unwrap())
    );

    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("Workspace"), "the pane renders:\n{frame}");
    assert!(frame.contains("zephyrproject"), "the path shows:\n{frame}");
    assert!(
        frame.contains("zephyr 4.1"),
        "the Project pane's versions field must report the environment:\n{frame}"
    );
    assert!(
        frame.contains("versions:"),
        "the versions field must be named:\n{frame}"
    );
    assert!(
        !frame.contains("source:"),
        "the detection source no longer has a field:\n{frame}"
    );

    // Enter on the Update Zephyr button confirms first (it rewrites the
    // shared workspace)… five rows past the checklist.
    app.focus = Focus::Workspace;
    for _ in 0..5 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmWorkspace { .. })
    ));
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("west update"),
        "the confirm quotes the command:\n{frame}"
    );

    // …and accepting runs it in the workspace with the derived environment:
    // ZEPHYR_BASE computed from the workspace (never required from the
    // shell) and the configured toolchain exported alongside it.
    app.handle(key(KeyCode::Char('y')));
    assert!(app.build.as_ref().unwrap().is_busy());
    let command = app.build.as_ref().unwrap().output.front().unwrap().clone();
    assert_eq!(command, "$ west update");
    let finished = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().last.is_some(),
        10,
    );
    assert!(finished);
    assert!(app.build.as_ref().unwrap().last.as_ref().unwrap().ok);
}

#[test]
fn the_startup_tool_report_counts_the_venvs_west_as_present() {
    // The regression this pins: the tool report checked `west` against
    // `PATH` and ran before the workspace was resolved, so a west living
    // in the workspace venv (the getting-started layout) drew a false
    // "not found" warning on every startup.
    let root =
        std::env::temp_dir().join(format!("chiptui-buildview-venvwest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    let home = root.join("home");
    let ws = workspace_under(&home, "zephyrproject");
    std::fs::write(
        root.join("chiptui.toml"),
        format!("[zephyr]\nworkspace = \"{}\"\n", ws.display()),
    )
    .unwrap();

    // main.rs order: the home seam is in place before bootstrap, so the
    // detection and tool report inside it resolve this workspace, not the
    // machine's real one.
    let mut app = App::new(&root);
    app.set_home_dir(&home);
    app.bootstrap();

    // The report resolved the workspace first and judged the venv's west
    // (a real file) instead of `PATH` --- no warning may name west as
    // missing (cmake/ninja may legitimately warn on the host).
    assert!(app.workspace.as_ref().unwrap().resolved.is_some());
    let messages: Vec<&str> = app
        .logs
        .visible(usize::MAX)
        .map(|entry| entry.message.as_str())
        .collect();
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("west") && message.contains("not found")),
        "the venv's west must not be reported missing:\n{}",
        messages.join("\n")
    );
}

#[test]
fn an_unconfigured_pane_shows_the_open_checklist_and_dim_buttons() {
    let (mut app, _root) = zephyr_app("ws-missing", None);

    let panel = app.workspace.as_ref().unwrap();
    assert!(panel.dir().is_none());
    assert!(panel.invalid.is_none(), "nothing was configured: no error");
    assert_eq!(
        panel.actions(&app.manager.capabilities()),
        vec![
            chiptui::workspace::WorkspaceAction::Choose,
            chiptui::workspace::WorkspaceAction::Projects,
            chiptui::workspace::WorkspaceAction::Project,
            chiptui::workspace::WorkspaceAction::Board,
            chiptui::workspace::WorkspaceAction::Shield,
            chiptui::workspace::WorkspaceAction::Update,
            chiptui::workspace::WorkspaceAction::SdkList
        ],
        "the checklist questions first, then the (disabled) buttons"
    );
    assert!(
        !panel.action_enabled(chiptui::workspace::WorkspaceAction::Update),
        "west update has nothing to run against yet"
    );

    // The checklist asks, the buttons stay visible but dim --- the state
    // explains itself without a separate guidance block.
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("□ Zephyr Base"),
        "the open question must show:\n{frame}"
    );
    assert!(
        frame.contains("Projects Base"),
        "the second question must show:\n{frame}"
    );
    assert!(
        frame.contains("↻ Update Zephyr"),
        "the button must stay visible:\n{frame}"
    );
}

#[test]
fn startup_asks_where_the_installation_is_when_nothing_is_configured() {
    let (mut app, root) = zephyr_app("ws-ask", None);
    let home = root.join("home");
    // A real installation exists under the (fixture) home: the picker's
    // target.
    let ws = workspace_under(&home, "myzephyr");

    app.maybe_open_workspace_picker();
    let Overlay::DirPicker { path, .. } = app.overlay.clone().unwrap() else {
        panic!("startup must ask immediately");
    };
    assert_eq!(path, home, "the picker starts at the user's home");

    // Navigate to the installation: Down past ".." onto it, Enter to
    // descend (which lands on "use this directory")…
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    // …and the reflex Enter accepts it.
    app.handle(key(KeyCode::Enter));
    assert!(
        app.overlay.is_none(),
        "a valid installation closes the picker"
    );

    // The choice was validated and persisted to the user config…
    let config = chiptui::settings::user_config_path(&home);
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(
        saved.contains(&format!("workspace = \"{}\"", ws.display())),
        "the pick must be saved where resolution reads it:\n{saved}"
    );
    // …and the session already runs against it: the pane resolved, and the
    // build panel's commands point at the venv's west.
    let panel = app.workspace.as_ref().unwrap();
    assert_eq!(panel.dir(), Some(&ws));
    assert!(
        app.build
            .as_ref()
            .unwrap()
            .tool_path()
            .unwrap()
            .starts_with(ws.join(".venv/bin/west").to_str().unwrap())
    );
}

#[test]
fn a_configured_location_never_re_asks() {
    let (mut app, root) = zephyr_app("ws-ask2", None);
    let home = root.join("home");
    let ws = workspace_under(&home, "myzephyr");
    chiptui::settings::save_workspace(&chiptui::settings::user_config_path(&home), &ws).unwrap();

    app.workspace = None;
    app.build = None;
    app.maybe_scan_devices();
    app.maybe_open_workspace_picker();

    assert!(app.overlay.is_none(), "the config answers, no picker");
    assert_eq!(
        app.workspace.as_ref().unwrap().dir(),
        Some(&ws),
        "resolution reads the saved location"
    );
}

#[test]
fn a_wrong_directory_is_rejected_with_the_install_guide() {
    let (mut app, _root) = zephyr_app("ws-wrong", None);

    app.maybe_open_workspace_picker();

    // Accept the home itself: not an installation (no .west/).
    app.handle(key(KeyCode::Enter));
    let Overlay::DirPicker { error, .. } = app.overlay.clone().unwrap() else {
        panic!("a rejection must keep the picker open");
    };
    let error = error.expect("the rejection must explain itself");
    assert!(error.contains(".west"), "names the marker: {error}");
    assert!(
        error.contains("docs.zephyrproject.org"),
        "points at the install guide: {error}"
    );

    // The pane is still unresolved after cancelling.
    app.handle(key(KeyCode::Esc));
    assert!(app.workspace.as_ref().unwrap().dir().is_none());
}

#[test]
fn a_configured_but_broken_location_reports_the_guide_and_still_lets_you_choose() {
    let (mut app, root) = zephyr_app("ws-broken", None);
    std::fs::write(
        root.join("chiptui.toml"),
        format!(
            "[zephyr]\nworkspace = \"{}\"\n",
            root.join("home/not-an-install").display()
        ),
    )
    .unwrap();
    app.workspace = None;
    app.build = None;
    app.maybe_scan_devices();

    // The pane reports the invalid location with the install guide…
    let panel = app.workspace.as_ref().unwrap();
    let message = panel.invalid.as_ref().unwrap();
    assert!(message.contains(".west"));
    assert!(message.contains("docs.zephyrproject.org"));
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("docs.zephyrproject.org"),
        "the guide must show:\n{frame}"
    );
    // …and does not auto-open the picker: the error is the answer's
    // context, the chooser is one Enter away.
    assert!(app.overlay.is_none());

    // Enter opens the directory picker from the pane.
    app.focus = Focus::Workspace;
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::DirPicker { .. })));
}

#[test]
fn menuconfig_hands_the_terminal_over_instead_of_piping() {
    let mut app = app_with_west("menuconfig", "west");
    app.focus = Focus::Build;

    // Menuconfig is the panel's first row.
    app.handle(key(KeyCode::Enter));

    let command = app.take_pending_command().expect("a parked command");
    assert!(command.to_string().ends_with("west build -t menuconfig"));
    assert!(
        !app.build.as_ref().unwrap().is_busy(),
        "nothing runs through the process manager"
    );
    assert!(app.take_pending_command().is_none(), "consumed once");
}

#[test]
fn the_build_dir_picker_switches_the_lifecycle_target() {
    let (mut app, root) = zephyr_app("builddir", Some("nrf52840dk/nrf52840"));
    app.build.as_mut().unwrap().set_tool_path(fake("west"));
    std::fs::create_dir_all(root.join("build-thingy/zephyr")).unwrap();
    std::fs::write(
        root.join("build-thingy/zephyr/CMakeCache.txt"),
        "CACHED_BOARD:STRING=thingy91/nrf9160\n",
    )
    .unwrap();
    app.focus = Focus::Build;

    // The panel's list no longer offers a Dir row (the lifecycle targets
    // the conventional `build` inside the project), but the picker and its
    // plumbing remain, reachable here directly for the switch itself.
    app.overlay = Some(Overlay::BuildDirPicker {
        input: String::new(),
        selected: 0,
    });

    // Filter to the configured directory and pick it.
    app.handle(key(KeyCode::Char('t')));
    app.handle(key(KeyCode::Char('h')));
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.build.as_ref().unwrap().build_dir,
        "build-thingy",
        "the pick lands"
    );
    assert_eq!(
        app.build.as_ref().unwrap().board_name(),
        Some("thingy91/nrf9160"),
        "the board answer follows the directory's cache"
    );

    // And the lifecycle commands now target it.
    let backend = app.manager.backend().unwrap();
    let clean = app
        .build
        .as_ref()
        .unwrap()
        .command(BuildKind::Clean, backend)
        .unwrap();
    assert!(
        clean
            .to_string()
            .ends_with("west build -d build-thingy -t clean")
    );
}
