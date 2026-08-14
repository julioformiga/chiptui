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
    std::fs::create_dir_all(root.join("build/zephyr")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    if let Some(board) = board {
        std::fs::write(
            root.join("build/zephyr/CMakeCache.txt"),
            format!("CACHED_BOARD:STRING={board}\n"),
        )
        .unwrap();
    }

    let mut app = App::new(&root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.maybe_scan_devices();
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

    // Tab tour: Local -> Build -> Logs -> Local.
    app.focus = Focus::Logs;
    app.handle(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::FilesLocal);
    app.handle(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Build);
    app.handle(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Logs);

    // And it renders, quoting board and commands.
    app.focus = Focus::Build;
    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("Build"), "missing panel:\n{frame}");
    assert!(
        frame.contains("nrf52840dk/nrf52840"),
        "cached board not shown:\n{frame}"
    );
    assert!(
        frame.contains("west build -t clean"),
        "literal commands not listed:\n{frame}"
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

    app.handle(key(KeyCode::Enter));

    assert!(app.build.as_ref().unwrap().is_busy());
    assert_eq!(app.view, View::Dashboard);
    assert_eq!(app.focus, Focus::Logs);
    assert_eq!(app.log_tab, LogTab::Monitor);
    assert_eq!(app.monitor_source, MonitorSource::Build);

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
    assert_eq!(last.kind, BuildKind::Build);
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

    app.handle(key(KeyCode::Down)); // Clean
    app.handle(key(KeyCode::Enter));

    // Destructive capability (Capability::Clean): a confirm quoting the
    // literal command, defaulting to No.
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmBuild {
            kind: BuildKind::Clean,
            confirm: false
        })
    );
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("west build -t clean"),
        "the confirm must quote the literal command:\n{frame}"
    );

    // Accept with 'y': the command runs.
    app.handle(key(KeyCode::Char('y')));
    assert!(app.build.as_ref().unwrap().is_busy());
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
    app2.handle(key(KeyCode::Down));
    app2.handle(key(KeyCode::Enter));
    app2.handle(key(KeyCode::Esc));
    assert!(!app2.build.as_ref().unwrap().is_busy());
}

#[test]
fn rebuild_is_pristine_and_pins_the_cached_board() {
    let mut app = app_with_west("rebuild", "west");
    app.focus = Focus::Build;

    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down)); // Rebuild
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

    app.handle(key(KeyCode::Enter));
    assert!(app.build.as_ref().unwrap().is_busy());

    // While running, Stop heads the list. Starting the build moved focus to
    // the Monitor tab; walk back to the panel (Logs -> Local -> Build) and
    // press Enter on Stop.
    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("Stop"), "no stop row:\n{frame}");
    app.handle(key(KeyCode::Tab));
    app.handle(key(KeyCode::Tab));
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

    app.handle(key(KeyCode::Enter));
    let finished = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().last.is_some(),
        10,
    );
    assert!(finished);
    let last = app.build.as_ref().unwrap().last.as_ref().unwrap();
    assert!(!last.ok, "a non-zero exit must be a failure");

    // The panel is idle again and a retry is possible.
    assert!(!app.build.as_ref().unwrap().is_busy());
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
