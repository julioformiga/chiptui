//! The busy-script paths end to end, against the fake `mpremote` variants.
//!
//! A board whose `main.py` sits in a blocking loop cannot be listed by
//! `mpremote` without interrupting it (Ctrl-C, then raw REPL), so ChipTUI
//! probes first, asks before interrupting, and offers to restart the script
//! afterwards --- the flows exercised here with an idle, a printing and a
//! silent fake board.

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::app::{App, Overlay};
use chiptui::backend::BackendKind;
use chiptui::browser::{Browser, PaneState};
use chiptui::device::{DeviceInfo, ScriptState};
use chiptui::event::AppEvent;
use chiptui::flash::FlashPanel;
use chiptui::process::ProcessManager;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn fake(tool: &str) -> String {
    format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
}

/// An app with a MicroPython override, a browser pointed at a fake mpremote,
/// and a fake esptool so the background device query stays deterministic.
fn app_with(tool: &str) -> App {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    let mut browser = Browser::new(std::env::temp_dir());
    browser.set_tool_path(fake(tool));
    app.browser = Some(browser);
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.set_tool_path(fake("esptool"));
    app.flash = Some(flash);
    app.maybe_scan_devices();
    // Pre-seeding the browser above means `ensure_browser_scanning` finds one
    // and skips its own scan start; 'd' issues it against the fake.
    app.handle(key(KeyCode::Char('d')));
    app
}

/// Drains process events into the app, advancing one tick per round so the
/// probe's deadline moves too, until `done` holds or time runs out.
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

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Renders the current screen and returns it as plain text.
fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| chiptui::ui::draw(frame, app))
        .unwrap();
    terminal.backend().to_string()
}

#[test]
fn the_interrupt_and_restore_prompts_render() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));

    let frame = render(&mut app, 110, 24);
    assert!(
        frame.contains("A script is running"),
        "the confirmation should say why it is asking:\n{frame}"
    );
    assert!(
        frame.contains("interrupts"),
        "the mechanism should be named:\n{frame}"
    );

    app.overlay = Some(Overlay::RestoreDeviceScript { selected: 2 });
    let frame = render(&mut app, 110, 24);
    assert!(
        frame.contains("Restart device script?"),
        "missing restore title:\n{frame}"
    );
    assert!(frame.contains("Reset the board"));
    assert!(frame.contains("Restart main.py"));
    assert!(frame.contains("Leave it stopped"));
}

fn pane(app: &App) -> &PaneState {
    &app.browser.as_ref().expect("browser exists").device_state
}

#[test]
fn an_idle_board_is_listed_without_asking_anything() {
    let mut app = app_with("mpremote");

    // Probe sees the banner and prompt, concludes "idle", and the listing
    // proceeds on its own.
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
    assert_eq!(app.devices.script_state(), ScriptState::Stopped);
    assert_eq!(app.overlay, None, "nothing needed the user's say-so");
}

#[test]
fn a_busy_board_holds_the_listing_behind_a_confirmation() {
    let mut app = app_with("mpremote-busy-board");

    assert!(
        pump_until(
            &mut app,
            |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
            20
        ),
        "the probe found the running script and asked"
    );
    assert!(
        app.browser.as_ref().unwrap().held_for_interrupt(),
        "the listing is held, not racing the script"
    );
    assert!(
        matches!(pane(&app), PaneState::Loading),
        "the pane is waiting, not failed"
    );
    assert_eq!(app.devices.script_state(), ScriptState::Running);
}

#[test]
fn declining_the_interruption_drops_the_listing() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));

    // Default is No; a reflex Enter must not stop the board's script.
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, None);
    assert!(matches!(pane(&app), PaneState::Failed(message) if message.contains("script")));
    assert_eq!(
        app.devices.script_state(),
        ScriptState::Running,
        "declining leaves the script alone"
    );
}

#[test]
fn accepting_the_interruption_lists_and_then_asks_how_to_restore() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));

    app.handle(key(KeyCode::Char('y')));

    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
    assert_eq!(
        app.devices.script_state(),
        ScriptState::Stopped,
        "the accepted interruption stopped the script"
    );

    // Once the accepted operations drain, the restore question appears.
    assert!(pump_until(
        &mut app,
        |app| matches!(
            app.overlay,
            Some(Overlay::RestoreDeviceScript { selected: 2 })
        ),
        20
    ));

    // Esc means "leave it stopped": a valid choice, no further prompts.
    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
    assert_eq!(app.devices.script_state(), ScriptState::Stopped);
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
}

#[test]
fn restoring_via_hard_reset_marks_the_script_running_again() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| matches!(
            app.overlay,
            Some(Overlay::RestoreDeviceScript { selected: 2 })
        ),
        20
    ));

    // Up twice: "leave it stopped" -> "restart main.py" -> "reset the board".
    app.handle(key(KeyCode::Up));
    app.handle(key(KeyCode::Up));
    app.handle(key(KeyCode::Enter));

    assert!(pump_until(
        &mut app,
        |app| app.devices.script_state() == ScriptState::Running,
        20
    ));
    // A reboot invalidates the old listing; the pane reloads.
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
}

#[test]
fn a_silent_board_is_listed_without_guessing_a_running_script() {
    let mut app = app_with("mpremote-quiet-board");

    // Nothing to see: the probe's window passes, it gives up, and the first
    // listing proceeds ungated --- the documented blind spot for scripts
    // that never print.
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
    assert_eq!(app.devices.script_state(), ScriptState::Unknown);
    assert_eq!(app.overlay, None);
}

#[test]
fn the_monitor_marks_a_printing_script_as_running() {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    let mut browser = Browser::new(std::env::temp_dir());
    browser.set_tool_path(fake("mpremote-busy-board"));
    app.browser = Some(browser);
    app.devices.set_devices(vec![DeviceInfo {
        port: "/dev/ttyACM0".into(),
        serial: None,
        vid_pid: "2e8a:0005".into(),
        description: "MicroPython Board".into(),
    }]);

    app.open_monitor();
    assert!(pump_until(
        &mut app,
        |app| app.devices.script_state() == ScriptState::Running,
        20
    ));
}

/// Drives the browser until no device command is outstanding, collecting
/// log messages and script-running flags along the way.
fn settle_collect(
    browser: &mut Browser,
    processes: &mut ProcessManager,
) -> (Vec<String>, Vec<bool>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut messages = Vec::new();
    let mut script_running = Vec::new();
    while browser.is_busy() && Instant::now() < deadline {
        for event in processes.drain() {
            let update = browser.on_process(&event, processes, None);
            messages.extend(update.notices.into_iter().map(|(_, text)| text));
            if let Some(running) = update.script_running {
                script_running.push(running);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!browser.is_busy(), "device command never completed");
    (messages, script_running)
}

#[test]
fn restore_commands_report_the_script_running_again() {
    let (mut browser, mut processes) = {
        let mut browser = Browser::new(std::env::temp_dir());
        browser.set_tool_path(fake("mpremote"));
        (browser, ProcessManager::new())
    };

    browser.request_hard_reset(&mut processes, None);
    let (messages, script_running) = settle_collect(&mut browser, &mut processes);
    assert!(
        messages
            .iter()
            .any(|m| m.contains("main.py will run again")),
        "hard reset should say what it did: {messages:?}"
    );
    assert_eq!(script_running, vec![true]);

    browser.request_relaunch_main(&mut processes, None);
    let (messages, script_running) = settle_collect(&mut browser, &mut processes);
    assert!(
        messages.iter().any(|m| m.contains("main.py restarted")),
        "relaunch should say what it did: {messages:?}"
    );
    assert_eq!(script_running, vec![true]);
}

#[test]
fn a_soft_reset_leaves_the_script_stopped() {
    let (mut browser, mut processes) = {
        let mut browser = Browser::new(std::env::temp_dir());
        browser.set_tool_path(fake("mpremote"));
        (browser, ProcessManager::new())
    };

    browser.request_reset(&mut processes, None);
    let (messages, script_running) = settle_collect(&mut browser, &mut processes);
    assert_eq!(
        script_running,
        vec![false],
        "the raw-REPL reboot skips main.py, and the reload re-interrupts besides"
    );
    assert!(messages.iter().any(|m| m.contains("reset")));
}

#[test]
fn the_device_info_query_runs_once_the_restore_choice_is_made() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));

    // The restore prompt is open: the esptool query must not fire underneath
    // a decision that may also want the port (a reset, say).
    assert!(matches!(
        app.overlay,
        Some(Overlay::RestoreDeviceScript { .. })
    ));
    assert!(
        !app.flash.as_ref().unwrap().is_busy(),
        "esptool raced the restore decision"
    );

    // "Leave it stopped" (Esc, the highlighted default) releases the device.
    app.handle(key(KeyCode::Esc));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.family.is_some()),
        20
    ));
}

#[test]
fn the_device_info_query_waits_while_the_script_runs() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));

    // Decline (default No + Enter): the user just said "do not interrupt",
    // and esptool resets the board to read the chip, so it must wait.
    app.handle(key(KeyCode::Enter));

    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        app.handle(AppEvent::Tick);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !app.flash.as_ref().unwrap().is_busy(),
        "esptool must not reset the board the user just protected"
    );
    assert!(
        app.flash.as_ref().unwrap().details.is_empty(),
        "no device info should have arrived"
    );

    // Once the script is no longer believed running (the monitor seeing an
    // idle REPL, say), the query goes ahead on its own.
    app.devices.set_script_state(ScriptState::Stopped);
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.family.is_some()),
        20
    ));
}
