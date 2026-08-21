//! The build panel end to end, against the fake `west`: the panel appears
//! for a build-capable backend without a filesystem, `Enter` runs the
//! composed command and streams into the Monitor tab, `Clean` asks first,
//! and `Stop` cancels a long-running command.

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::app::{App, Focus, LogTab, MonitorSource, Overlay, View};
use chiptui::backend::BackendKind;
use chiptui::backend::BuildKind;
use chiptui::build::BuildAction;
use chiptui::event::AppEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn fake(tool: &str) -> String {
    format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> KeyCode {
    KeyCode::Char(c)
}

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, modifiers))
}

/// The Project pane's way in: the shortcuts overlay (`ctrl+k`), then the
/// pane's `e` letter.
fn enter_project_pane(app: &mut App) {
    app.handle(key_event(ctrl('k'), KeyModifiers::CONTROL));
    app.handle(key(KeyCode::Char('e')));
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

/// Walks the build panel's cursor onto `action`, so a test names the row it
/// means instead of counting `Down` presses --- the count moves whenever the
/// action list does, and a miscount lands on a neighbouring row silently.
fn cursor_on(app: &mut App, action: BuildAction) {
    let caps = app.manager.capabilities();
    let panel = app.build.as_mut().expect("a build panel");
    let target = panel
        .actions(&caps)
        .iter()
        .position(|candidate| *candidate == action)
        .unwrap_or_else(|| panic!("{action:?} is not in the action list"));
    panel.cursor = target;
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
    // (140 columns: the merged Board · Shield row shows both names in
    // full --- at 100 the shared value column tail-truncates one of them.)
    app.focus = Focus::Build;
    let frame = render(&mut app, 140, 32);
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

    cursor_on(&mut app, BuildAction::Build(BuildKind::Build));
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
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("fake west: build"),
        "output missing:\n{frame}"
    );
}

/// Item 05 of the 2026-08-20 UX audit: the state line must show ninja's own
/// step counter while it streams in, not just a stopwatch, and go back to
/// the counted duration once the command finishes.
#[test]
fn a_running_build_reports_ninjas_step_counter() {
    let mut app = app_with_west("progress", "west-progress");
    app.focus = Focus::Build;

    cursor_on(&mut app, BuildAction::Build(BuildKind::Build));
    app.handle(key(KeyCode::Enter));
    assert!(app.build.as_ref().unwrap().is_busy());

    let caught_progress = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().progress().is_some(),
        10,
    );
    assert!(caught_progress, "no ninja progress line was ever parsed");
    assert_eq!(
        app.build.as_ref().unwrap().progress(),
        Some(chiptui::progress::Progress::Steps { done: 1, total: 3 })
    );
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Build · 1/3"),
        "state line missing the step counter:\n{frame}"
    );

    let finished = pump_until(
        &mut app,
        |app| app.build.as_ref().unwrap().last.is_some(),
        10,
    );
    assert!(finished, "the fake west never finished");
    assert!(
        app.build.as_ref().unwrap().progress().is_none(),
        "progress must clear once the command is no longer running"
    );
}

#[test]
fn clean_asks_before_running() {
    let mut app = app_with_west("clean", "west");
    app.focus = Focus::Build;

    cursor_on(&mut app, BuildAction::Build(BuildKind::Clean));
    app.handle(key(KeyCode::Enter));

    // Destructive capability (Capability::Clean): a confirm quoting the
    // literal command, defaulting to No.
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmBuild {
            action: chiptui::build::BuildAction::Build(BuildKind::Clean),
            confirm: false
        })
    );
    let frame = render(&mut app, 100, 32);
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
    cursor_on(&mut app2, BuildAction::Build(BuildKind::Clean));
    app2.handle(key(KeyCode::Enter));
    app2.handle(key(KeyCode::Esc));
    assert!(!app2.build.as_ref().unwrap().is_busy());
}

#[test]
fn rebuild_is_pristine_and_pins_the_cached_board() {
    let mut app = app_with_west("rebuild", "west");
    app.focus = Focus::Build;

    cursor_on(&mut app, BuildAction::Build(BuildKind::Rebuild));
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

    // The pane's height must not move when the command starts: the
    // footer's rows are reserved even while idle, so every pane border
    // (and the log pane below) sits on the same row as before. Rendered
    // at 31 rows: row 1's fixed four content rows make the unclipped
    // layout (header + info + the build pane's full stack + the log
    // pane's minimum + footer) exactly this tall.
    let idle = render(&mut app, 100, 32);

    cursor_on(&mut app, BuildAction::Build(BuildKind::Build));
    app.handle(key(KeyCode::Enter));
    assert!(app.build.as_ref().unwrap().is_busy());
    let running_frame = render(&mut app, 100, 32);
    // Height constancy: the structural borders --- every pane's bottom
    // rule starts the line with `╰` (the Stop box's own rules are indented
    // into the pane's right half, so they never match) --- must sit on the
    // same rows as before the command started.
    let border_rows = |frame: &str| -> Vec<usize> {
        frame
            .lines()
            .enumerate()
            .filter(|(_, line)| line.starts_with('╰'))
            .map(|(index, _)| index)
            .collect()
    };
    assert_eq!(
        border_rows(&idle),
        border_rows(&running_frame),
        "the pane layout must not change when a command starts:\n{idle}\n---\n{running_frame}"
    );

    // While running, Stop trails the list as the footer box and holds the
    // cursor (starting the build left focus on the panel): Enter cancels.
    // The footer hugs the stack --- the box's top rule directly under the
    // stack's bottom rule, no blank row between Flash and Stop --- and is
    // split horizontally: the state on the left half, the box on the right
    // half of the pane (itself the terminal's right half, so the glyph
    // lands past the third quarter of the line), both on the same row.
    let frame = running_frame;
    let lines: Vec<&str> = frame.lines().collect();
    let stop_idx = lines
        .iter()
        .position(|line| line.contains("■ Stop"))
        .unwrap_or_else(|| panic!("no stop box:\n{frame}"));
    assert!(
        lines[stop_idx - 1].contains("╭"),
        "the box's top rule must sit directly above the label:\n{frame}"
    );
    assert!(
        lines[stop_idx - 2].contains("╰"),
        "the stack's bottom rule must sit directly above the box --- no blank row between Flash and Stop:\n{frame}"
    );
    let stop_x = lines[stop_idx].find("■ Stop").unwrap();
    assert!(
        stop_x > 75,
        "the Stop box must sit in the pane's right half ({stop_x}):\n{frame}"
    );
    assert!(
        lines[stop_idx].contains("running ·"),
        "the state must share the footer row, beside the box:\n{frame}"
    );
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

    // Idle again: the box is gone, the stack whole.
    let frame = render(&mut app, 100, 32);
    assert!(
        !frame.contains("■ Stop"),
        "no Stop box may show while idle:\n{frame}"
    );
    assert!(
        frame.contains("⇧ Flash"),
        "the stack's tail must be back:\n{frame}"
    );
}

#[test]
fn a_failed_command_reports_and_keeps_the_panel_usable() {
    // `noisy` writes to stderr and exits non-zero (see fixtures).
    let mut app = app_with_west("fail", "noisy");
    app.focus = Focus::Build;

    cursor_on(&mut app, BuildAction::Build(BuildKind::Build));
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

    // The real path: the empty-project prompt's answer applies the backend
    // and re-clamps (MicroPython is the prompt's first row).
    app.overlay = Some(Overlay::ProjectSetup { selected: 0 });
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

    // The Board row lives in the Project pane (the environment
    // checklist), below the other three questions.
    enter_project_pane(&mut app);
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
    let frame = render(&mut app, 100, 32);
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
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("nrf52840dk/nrf52840"),
        "filter failed:\n{frame}"
    );
    assert!(
        !frame.contains("native/native64"),
        "a non-matching target must be filtered out:\n{frame}"
    );

    // …and Enter picks: commands carry -b, the answer is saved in the
    // project's registry entry, and nothing is written into the project
    // itself.
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

    let frame = render(&mut app, 140, 32);
    assert!(
        frame.contains("picked"),
        "the row must say the board's origin:\n{frame}"
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
    enter_project_pane(&mut app);

    // The Board · Shield row is the fourth question; `→` switches the row's
    // segment to the shield half, which Enter then acts on.
    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Right));
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Shield"),
        "the target row must show:\n{frame}"
    );
    assert!(
        frame.contains("none"),
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
    let frame = render(&mut app, 100, 32);
    assert!(frame.contains("nrf7002ek"), "list not shown:\n{frame}");
    assert!(
        frame.contains("(none)"),
        "the clear row must show:\n{frame}"
    );

    // Typing filters; the first Down steps past the (none) row onto the
    // first match, and Enter picks.
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

    // The row shows the answer, and the (none) row clears it. (Focus never
    // left the Project pane's target row, and its segment is still the
    // shield half --- the `e` letter is a no-op while already focused.)
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("nrf7002ek"),
        "the answer must show:\n{frame}"
    );
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
fn board_and_shield_picks_are_saved_and_reloaded_with_the_project() {
    let (mut app, root) = zephyr_app("persist", None);
    app.build.as_mut().unwrap().set_tool_path(fake("west"));
    let config = root.join("home/.config/chiptui/config.toml");

    // Pick a board through the picker (three downs to the target row).
    enter_project_pane(&mut app);
    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
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
    for c in ['N', 'R', 'F'] {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(
        written.contains("board = \"nrf52840dk/nrf52840\""),
        "the pick must be saved in the registry entry:\n{written}"
    );

    // Pick a shield on the same row's right half.
    app.handle(key(KeyCode::Right));
    app.handle(key(KeyCode::Enter));
    assert!(pump_until(
        &mut app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().shields.state,
                chiptui::build::ListState::Loaded(_)
            )
        },
        10
    ));
    for c in ['n', 'r', 'f', '7'] {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(
        written.contains("shield = \"nrf7002ek\""),
        "the shield answer is saved beside the board:\n{written}"
    );

    // Clearing the shield persists the clearing: the saved answer is the
    // source of truth, not just the session state. (The cursor and its
    // shield segment never moved, and the picker starts on the (none) row.)
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Enter));
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(
        !written.contains("shield ="),
        "a cleared shield must not survive in the file:\n{written}"
    );
    assert!(
        written.contains("board = \"nrf52840dk/nrf52840\""),
        "clearing the shield must not disturb the board:\n{written}"
    );

    // A later session over the same project and home reloads the answers:
    // the board outranks the (here absent) build cache, with its origin
    // saying where it came from.
    let mut reopened = App::new(&root);
    reopened.set_serial_dir(root.join("dev"));
    reopened.set_home_dir(root.join("home"));
    reopened.bootstrap();
    reopened.manager.set_override(Some(BackendKind::Zephyr));
    reopened.maybe_scan_devices();
    let panel = reopened.build.as_ref().unwrap();
    assert_eq!(
        panel.board_name(),
        Some("nrf52840dk/nrf52840"),
        "the saved board must reload on open"
    );
    assert_eq!(
        panel.board.as_ref().unwrap().origin,
        chiptui::build::BoardOrigin::Config
    );
    assert_eq!(
        panel.shield_name(),
        None,
        "the cleared shield stays cleared"
    );

    // Recording the open --- what every project start does --- must keep
    // the answers rather than rewriting the entry without them.
    reopened.record_open_project();
    let rewritten = std::fs::read_to_string(&config).unwrap();
    assert!(
        rewritten.contains("board = \"nrf52840dk/nrf52840\""),
        "recording the open must not forget the board:\n{rewritten}"
    );
}

#[test]
fn enter_in_the_workspace_file_section_descends_directly() {
    let (mut app, root) = zephyr_app("open-dir", None);
    std::fs::create_dir_all(root.join("src")).unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    // The pane is all file list now (the checklist moved to the Project
    // pane): downs walk the entries, where `src` sits third --- behind the
    // fixture's `dev`/`home` seam directories, which sort ahead of it
    // (directories first, then alphabetically).
    for _ in 0..2 {
        app.handle(key(KeyCode::Down));
    }
    assert_eq!(
        app.workspace
            .as_ref()
            .unwrap()
            .files_selected()
            .unwrap()
            .name,
        "src"
    );

    // Enter descends straight into the directory --- there is no action
    // menu between the keypress and the navigation anymore. The listing
    // inside starts on the `[..]` row, selected.
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, None);
    let panel = app.workspace.as_ref().unwrap();
    assert_eq!(panel.files_path, root.join("src"));
    assert!(panel.on_parent_row(), "the [..] row must lead, selected");

    // Enter on `[..]` steps back up, leaving "src" selected in its parent.
    app.handle(key(KeyCode::Enter));
    let panel = app.workspace.as_ref().unwrap();
    assert_eq!(panel.files_path, root);
    assert!(!panel.on_parent_row());
    assert_eq!(panel.files_selected().unwrap().name, "src");
}

/// Walks the workspace pane's file cursor onto the entry named `name`
/// (assumed to exist), asserting it landed there. Only walks down; to reach
/// an entry above the cursor, close what is open and walk up instead.
fn workspace_cursor_on(app: &mut App, name: &str) {
    for _ in 0..32 {
        let panel = app.workspace.as_ref().unwrap();
        if panel.files_selected().is_some_and(|e| e.name == name) {
            return;
        }
        app.handle(key(KeyCode::Down));
    }
    panic!("never reached the {name} entry");
}

#[test]
fn the_workspace_file_section_titles_with_the_project_and_offers_the_parent_row() {
    let (mut app, root) = zephyr_app("section-title", None);
    std::fs::create_dir_all(root.join("src")).unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    // At the root the pane's title carries the project's own name… (140
    // columns: the fixture's generated name is long, and the
    // "Files: " prefix eats 7 of them.)
    let project = root.file_name().unwrap().to_string_lossy().into_owned();
    let frame = render(&mut app, 140, 32);
    assert!(
        frame.contains(&format!("{project}/")),
        "the title bar must name the project:\n{frame}"
    );
    assert!(
        !frame.contains("[..]"),
        "no parent row at the project root:\n{frame}"
    );

    // …and descending concatenates the walked path onto it, over a listing
    // that leads with `[..]`.
    workspace_cursor_on(&mut app, "src");
    app.handle(key(KeyCode::Enter));
    let frame = render(&mut app, 140, 32);
    assert!(
        frame.contains(&format!("{project}/src/")),
        "the title must concatenate the descent:\n{frame}"
    );
    assert!(frame.contains("[..]"), "the parent row must lead:\n{frame}");
}

/// The upward mirror of [`workspace_cursor_on`].
fn workspace_cursor_up_to(app: &mut App, name: &str) {
    for _ in 0..32 {
        let panel = app.workspace.as_ref().unwrap();
        if panel.files_selected().is_some_and(|e| e.name == name) {
            return;
        }
        app.handle(key(KeyCode::Up));
    }
    panic!("never reached the {name} entry");
}

#[test]
fn enter_on_a_text_file_in_the_workspace_file_section_queues_the_editor() {
    let (mut app, root) = zephyr_app("edit-file", None);
    std::fs::write(root.join("main.c"), "int main(void) { return 0; }\n").unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "main.c");
    app.handle(key(KeyCode::Enter));

    // No menu: Enter on an editable file goes straight to $EDITOR.
    assert_eq!(app.overlay, None);
    let pending = app
        .take_pending_edit()
        .expect("Enter on a text file must queue $EDITOR");
    assert_eq!(pending.path, root.join("main.c"));
    assert_eq!(
        pending.device_target, None,
        "a local edit has no device half"
    );
}

#[test]
fn enter_and_v_on_a_binary_workspace_file_do_nothing() {
    let (mut app, root) = zephyr_app("binary-file", None);
    std::fs::write(root.join("zephyr.bin"), [0u8, 1, 2, 3]).unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "zephyr.bin");
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Char('v')));

    assert_eq!(app.overlay, None);
    assert!(app.viewer.is_none());
    assert!(app.take_pending_edit().is_none());
    assert!(
        root.join("zephyr.bin").exists(),
        "nothing was deleted either"
    );
}

#[test]
fn v_on_a_text_file_in_the_workspace_file_section_opens_the_viewer() {
    let (mut app, root) = zephyr_app("view-file", None);
    std::fs::write(root.join("prj.conf"), "CONFIG_SERIAL=y\n").unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "prj.conf");
    app.handle(key(KeyCode::Char('v')));

    assert_eq!(app.overlay, Some(Overlay::FileViewer));
    let viewer = app.viewer.as_ref().unwrap();
    assert_eq!(
        viewer.source,
        chiptui::app::ViewerSource::Local(root.join("prj.conf"))
    );
    assert!(matches!(
        viewer.state,
        chiptui::app::ViewerState::Ready { .. }
    ));

    // `v` is a file action: on a directory it does nothing. Close the
    // viewer, walk up to the `dev` seam directory and try again.
    app.handle(key(KeyCode::Esc));
    workspace_cursor_up_to(&mut app, "dev");
    app.handle(key(KeyCode::Char('v')));
    assert_eq!(app.overlay, None);
    assert!(app.viewer.is_none());
}

#[test]
fn del_in_the_workspace_file_section_asks_first_and_defaults_to_no() {
    let (mut app, root) = zephyr_app("del-file", None);
    std::fs::write(root.join("prj.conf"), "CONFIG_SERIAL=y\n").unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "prj.conf");

    // Del asks, with No highlighted: a reflex Enter declines and the file
    // survives.
    app.handle(key(KeyCode::Delete));
    match app.overlay {
        Some(Overlay::ConfirmDelete {
            name: ref file,
            confirm: false,
            ..
        }) => assert_eq!(file, "prj.conf"),
        other => panic!("expected a default-No delete confirmation, got {other:?}"),
    }
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, None);
    assert!(
        root.join("prj.conf").exists(),
        "a reflex Enter must not delete"
    );

    // Answering Yes (here via `y`) deletes and refreshes the listing.
    app.handle(key(KeyCode::Delete));
    app.handle(key(KeyCode::Char('y')));
    assert_eq!(app.overlay, None);
    assert!(!root.join("prj.conf").exists());
    assert!(
        !app.workspace
            .as_ref()
            .unwrap()
            .files_entries
            .iter()
            .any(|entry| entry.name == "prj.conf"),
        "the listing must drop the deleted entry"
    );
}

#[test]
fn r_in_the_workspace_file_section_opens_a_prefilled_rename_prompt() {
    let (mut app, root) = zephyr_app("rename-open", None);
    std::fs::write(root.join("main.c"), "int main(void) { return 0; }\n").unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "main.c");
    app.handle(key(KeyCode::Char('r')));
    assert_eq!(
        app.overlay,
        Some(Overlay::RenameEntry {
            name: "main.c".to_string(),
            input: "main.c".to_string(),
        }),
        "the prompt must open pre-filled with the current name"
    );

    let frame = render(&mut app, 100, 32);
    assert!(frame.contains("Rename"), "textbox not shown:\n{frame}");
    assert!(
        frame.contains("current name: main.c"),
        "the current name must be visible:\n{frame}"
    );

    // An unedited confirm is a quiet no-op, not an error.
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, None);
    assert!(root.join("main.c").is_file());
}

#[test]
fn renaming_a_workspace_file_moves_it_and_refreshes_the_list() {
    let (mut app, root) = zephyr_app("rename-file", None);
    std::fs::write(root.join("main.c"), "int main(void) { return 0; }\n").unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "main.c");
    app.handle(key(KeyCode::Char('r')));
    // The pre-filled field edits from its end: drop the extension, type a
    // new one.
    for _ in 0..2 {
        app.handle(key(KeyCode::Backspace));
    }
    for c in "_v2.c".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, None);
    assert!(!root.join("main.c").exists());
    assert!(root.join("main_v2.c").is_file());
    assert!(
        app.workspace
            .as_ref()
            .unwrap()
            .files_entries
            .iter()
            .any(|entry| entry.name == "main_v2.c"),
        "the listing must refresh with the new name"
    );
}

#[test]
fn renaming_a_workspace_directory_works_the_same_way() {
    let (mut app, root) = zephyr_app("rename-dir", None);
    std::fs::create_dir_all(root.join("src")).unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "src");
    app.handle(key(KeyCode::Char('r')));
    for _ in 0..3 {
        app.handle(key(KeyCode::Backspace));
    }
    for c in "boards".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, None);
    assert!(!root.join("src").exists());
    assert!(root.join("boards").is_dir());
}

#[test]
fn escaping_rename_leaves_the_entry_alone() {
    let (mut app, root) = zephyr_app("rename-escape", None);
    std::fs::write(root.join("prj.conf"), "CONFIG_SERIAL=y\n").unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "prj.conf");
    app.handle(key(KeyCode::Char('r')));
    for _ in 0..8 {
        app.handle(key(KeyCode::Backspace));
    }
    app.handle(key(KeyCode::Esc));

    assert_eq!(app.overlay, None);
    assert!(
        root.join("prj.conf").is_file(),
        "Esc must leave the entry untouched"
    );
}

#[test]
fn renaming_into_a_path_is_refused() {
    let (mut app, root) = zephyr_app("rename-slash", None);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("main.c"), "int main(void) { return 0; }\n").unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "main.c");
    app.handle(key(KeyCode::Char('r')));
    for _ in 0..6 {
        app.handle(key(KeyCode::Backspace));
    }
    for c in "src/main.c".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));

    // A `/` would turn the rename into a move, so it is refused --- the
    // file stays where and what it was.
    assert_eq!(app.overlay, None);
    assert!(root.join("main.c").is_file());
    assert!(!root.join("src/main.c").exists());
}

#[test]
fn del_on_a_workspace_directory_asks_before_removing_it_recursively() {
    let (mut app, root) = zephyr_app("del-dir", None);
    std::fs::create_dir_all(root.join("src/sub")).unwrap();
    std::fs::write(root.join("src/sub/file.c"), "x\n").unwrap();
    app.workspace.as_mut().unwrap().reload_files();
    app.focus = Focus::Workspace;

    workspace_cursor_on(&mut app, "src");
    app.handle(key(KeyCode::Delete));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmDelete {
            is_dir: true,
            confirm: false,
            ..
        })
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(!root.join("src").exists(), "the directory tree is removed");
}

#[test]
fn a_boardless_filter_match_enter_picks_nothing_and_esc_changes_nothing() {
    let (mut app, _root) = zephyr_app("picker-esc", Some("nrf52840dk/nrf52840"));
    app.build.as_mut().unwrap().set_tool_path(fake("west"));
    enter_project_pane(&mut app);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    } // the Board · Shield row
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

    // …and Esc leaves the cache answer untouched either way. (Focus never
    // left the Project pane: the `e` letter is a no-op while already
    // focused.)
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
    enter_project_pane(&mut app);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    } // the Board · Shield row
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
    let frame = render(&mut app, 100, 32);
    // The explanation wraps with the modal's width, so the assertion is
    // wrap-agnostic: both halves of the sentence must be on screen.
    assert!(
        frame.contains("is west on") && frame.contains("PATH?"),
        "the picker must explain the failure:\n{frame}"
    );
}

#[test]
fn flash_is_listed_confirms_and_runs_through_west() {
    let mut app = app_with_west("flash", "west");
    app.focus = Focus::Build;

    // Flash sits last: Update Zephyr, SDK List, Menuconfig, Clean, Build,
    // Rebuild, then it.
    for _ in 0..6 {
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
    let frame = render(&mut app, 100, 32);
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
fn x_routes_a_build_backend_to_west_flash_and_micropython_to_the_actions_tab() {
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

    // MicroPython: `x` opens the device pane's Actions tab --- the
    // esptool menu's new home --- not a dialog.
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.handle(key(KeyCode::Char('x')));
    assert_ne!(app.view, View::Flash);
    assert!(
        app.device_actions_tab_active(),
        "the actions tab is showing"
    );
    assert_eq!(app.focus, Focus::FilesDevice);
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
    // Where row 2 starts before the workspace resolves: resolving adds the
    // Project pane's versions badge, which must not move anything (row 1 is
    // a fixed four content rows).
    let unresolved = render(&mut app, 100, 32);
    let row2 = unresolved
        .lines()
        .position(|l| l.contains("Files"))
        .expect("the project-files pane renders before resolution");

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

    let frame = render(&mut app, 100, 32);
    assert!(frame.contains("Files"), "the pane renders:\n{frame}");
    assert!(frame.contains("zephyrproject"), "the path shows:\n{frame}");
    assert!(
        frame.contains("zephyr 4.1"),
        "the versions badge must report the environment:\n{frame}"
    );
    assert!(
        !frame.contains("versions:"),
        "the badge carries no label --- the values name themselves:\n{frame}"
    );
    assert_eq!(
        frame.lines().position(|l| l.contains("Files")),
        Some(row2),
        "the versions badge must not shift row 2:\n{frame}"
    );
    assert!(
        !frame.contains("source:"),
        "the detection source no longer has a field:\n{frame}"
    );

    // Enter on the Update Zephyr/SDK button asks what to update first --- it
    // leads the Project actions pane now, the list's first row. Picking
    // "Update Zephyr" (the default, row 0) confirms next, since it rewrites
    // the shared workspace.
    app.focus = Focus::Build;
    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::UpdateZephyrChoice { selected: 0 })
    ));
    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmBuild {
            action: chiptui::build::BuildAction::UpdateZephyr,
            ..
        })
    ));
    let frame = render(&mut app, 100, 32);
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
fn the_update_choice_menu_routes_to_the_sdk_toolchain_picker() {
    // The other branch of the choice menu: picking "Update / add SDK
    // toolchains" must land on the same picker the `s` shortcut opens,
    // never on the `west update` confirm.
    let (mut app, root) = zephyr_app("ws-sdk", None);
    let home = root.join("home");
    let ws = workspace_under(&home, "zephyrproject");
    std::fs::write(
        root.join("chiptui.toml"),
        format!("[zephyr]\nworkspace = \"{}\"\n", ws.display()),
    )
    .unwrap();
    app.workspace = None;
    app.build = None;
    app.maybe_scan_devices();
    assert!(app.workspace.as_ref().unwrap().resolved.is_some());

    app.focus = Focus::Build;
    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::UpdateZephyrChoice { selected: 0 })
    ));
    app.handle(key(KeyCode::Down));
    assert!(matches!(
        app.overlay,
        Some(Overlay::UpdateZephyrChoice { selected: 1 })
    ));
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::SdkToolchains { .. })));
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
            chiptui::workspace::WorkspaceAction::BoardShield,
        ],
        "the checklist is the pane's whole action list now"
    );
    assert!(
        !app.build_action_enabled(chiptui::build::BuildAction::UpdateZephyr),
        "west update has nothing to run against yet"
    );
    // With nothing resolved, the environment row is the *other* half of
    // its slot: there is nothing to update, and installing is what the
    // user actually needs.
    assert_eq!(
        app.build
            .as_ref()
            .unwrap()
            .actions(&app.manager.capabilities())
            .first(),
        Some(&chiptui::build::BuildAction::InstallZephyr),
        "an unresolved workspace offers Install, not Update"
    );

    // The checklist asks, the buttons stay visible but dim --- the state
    // explains itself without a separate guidance block.
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("□ Zephyr path"),
        "the open question must show:\n{frame}"
    );
    assert!(
        frame.contains("Projects base"),
        "the second question must show:\n{frame}"
    );
    assert!(
        frame.contains("⇩ Install Zephyr"),
        "the environment button must stay visible:\n{frame}"
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
    let config = chiptui::settings::user_config_path(&chiptui::settings::config_dir_in(&home));
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
    chiptui::settings::save_workspace(
        &chiptui::settings::user_config_path(&chiptui::settings::config_dir_in(&home)),
        &ws,
    )
    .unwrap();

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

    // Accept the home itself: not an installation (no .west/). The
    // rejection now arrives with a way forward on top of it --- install one
    // here --- so declining that offer is what reveals the picker again.
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::ConfirmInstallHere { .. })),
        "a refused directory must offer to become an installation"
    );
    app.handle(key(KeyCode::Char('n')));
    let Overlay::DirPicker { error, .. } = app.overlay.clone().unwrap() else {
        panic!("declining the offer must leave the picker open");
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
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("docs.zephyrproject.org"),
        "the guide must show (in the log, the pane's rows are fixed):\n{frame}"
    );
    // …and does not auto-open the picker: the error is the answer's
    // context, the chooser is one Enter away.
    assert!(app.overlay.is_none());

    // Enter opens the directory picker from the pane (the `e` letter lands
    // on the first open question, which an invalid location still is).
    enter_project_pane(&mut app);
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::DirPicker { .. })));
}

#[test]
fn menuconfig_hands_the_terminal_over_instead_of_piping() {
    let mut app = app_with_west("menuconfig", "west");
    app.focus = Focus::Build;

    cursor_on(&mut app, BuildAction::Menuconfig);
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
