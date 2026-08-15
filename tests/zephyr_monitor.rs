//! The Zephyr monitor end to end: USB serial port discovery without
//! mpremote, the device picker for several ports, and `west monitor` in a
//! PTY session (`SPEC.md` §10's monitor, wired to the Monitor tab).

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::app::{App, Focus, LogTab, MonitorSource, Overlay};
use chiptui::backend::BackendKind;
use chiptui::device::DiscoveryState;
use chiptui::event::AppEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn fake(tool: &str) -> String {
    format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A Zephyr app whose `/dev` is a fixture directory the test fills.
fn zephyr_app(tag: &str) -> (App, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("chiptui-zmon-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();

    let mut app = App::new(&root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.set_serial_dir(root.join("dev"));
    // Workspace discovery must not look at the machine's real $HOME (a
    // ~/zephyrproject there would resolve the pane differently).
    std::fs::create_dir_all(root.join("home")).unwrap();
    app.set_home_dir(root.join("home"));
    app.maybe_scan_devices();
    // After the scan: `maybe_scan_devices` is what creates the build panel
    // the override belongs to.
    if let Some(panel) = app.build.as_mut() {
        panel.set_tool_path(fake("west"));
    }
    (app, root)
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

#[test]
fn a_single_usb_port_selects_itself_at_startup() {
    let (mut app, root) = zephyr_app("single");
    assert_eq!(
        app.devices.discovery,
        DiscoveryState::Failed,
        "empty fixture"
    );

    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));

    assert_eq!(app.devices.discovery, DiscoveryState::Ready);
    let expected = root.join("dev/ttyACM0").display().to_string();
    assert_eq!(
        app.devices.selected().map(|d| d.port.clone()),
        Some(expected),
        "one port is not a guess: it selects itself"
    );
    // The header names it.
    assert!(app.devices.summary().contains("ttyACM0"));
    // No subprocess was issued: the scan is a plain directory walk.
    assert!(app.processes.drain().is_empty());
}

#[test]
fn several_ports_ask_before_any_is_used() {
    let (mut app, root) = zephyr_app("multi");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    std::fs::write(root.join("dev/ttyACM1"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));

    assert!(app.devices.needs_selection());
    assert!(matches!(app.overlay, Some(Overlay::DevicePicker { .. })));

    // Choosing applies the port without any mpremote follow-through ---
    // there is no filesystem to list and no esptool to query on this
    // backend; the pick is the whole job.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    let expected = root.join("dev/ttyACM1").display().to_string();
    assert_eq!(
        app.devices.selected().map(|d| d.port.clone()),
        Some(expected)
    );
    assert!(
        app.processes.drain().is_empty(),
        "no probe, no listing, no query"
    );
}

#[test]
fn m_starts_west_monitor_in_a_pty_on_the_selected_port() {
    let (mut app, root) = zephyr_app("monitor");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert!(app.devices.selected_port().is_some());

    app.handle(key(KeyCode::Char('m')));

    // The session owns the Monitor tab and receives keystrokes.
    assert!(
        app.device_monitor_process.is_some(),
        "monitor did not spawn"
    );
    assert_eq!(app.focus, Focus::Logs);
    assert_eq!(app.log_tab, LogTab::Monitor);
    assert_eq!(app.monitor_source, MonitorSource::Device);

    // The fake west echoes its arguments (then exits; the session ends on
    // its own, which the Finished event handles without corrupting state).
    let echoed = pump_until(
        &mut app,
        |app| {
            app.device_monitor_output
                .iter()
                .any(|l| l.contains("monitor"))
        },
        10,
    );
    assert!(echoed, "no west monitor output arrived");
    let port = root.join("dev/ttyACM0").display().to_string();
    assert!(
        app.device_monitor_output
            .iter()
            .any(|l| l.contains(&port) && l.contains("--port")),
        "the monitor must name the selected port: {:?}",
        app.device_monitor_output
    );
}

#[test]
fn backend_picker_does_not_clobber_the_device_picker_it_opens() {
    let root = std::env::temp_dir().join(format!("chiptui-zmon-picker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    std::fs::write(root.join("dev/ttyACM1"), b"").unwrap();

    let mut app = App::new(&root);
    app.bootstrap();
    app.set_serial_dir(root.join("dev"));

    app.handle(key(KeyCode::Char('o')));
    assert!(matches!(app.overlay, Some(Overlay::BackendPicker { .. })));
    // Automatic, MicroPython, Zephyr --- two Down presses reach Zephyr.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));

    assert!(
        matches!(app.overlay, Some(Overlay::DevicePicker { .. })),
        "the device picker opened by picking Zephyr must survive the \
         backend picker's own Enter handler, got {:?}",
        app.overlay
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn project_setup_does_not_clobber_the_device_picker_it_opens() {
    let root = std::env::temp_dir().join(format!("chiptui-zmon-setup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    std::fs::write(root.join("dev/ttyACM1"), b"").unwrap();

    let mut app = App::new(&root);
    app.bootstrap();
    app.set_serial_dir(root.join("dev"));
    app.maybe_open_project_setup();
    assert!(matches!(app.overlay, Some(Overlay::ProjectSetup { .. })));

    // MicroPython, Zephyr --- one Down press reaches Zephyr.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));

    assert!(
        matches!(app.overlay, Some(Overlay::DevicePicker { .. })),
        "the device picker opened by picking Zephyr must survive the \
         project-setup overlay's own Enter handler, got {:?}",
        app.overlay
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rescanning_the_same_backend_does_not_reset_the_selected_port() {
    let (mut app, root) = zephyr_app("rescan");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));

    assert_eq!(app.devices.discovery, DiscoveryState::Ready);
    let selected_before = app.devices.selected_port().map(str::to_string);
    assert!(selected_before.is_some());

    app.maybe_scan_devices();

    assert_eq!(
        app.devices.discovery,
        DiscoveryState::Ready,
        "a second scan must not cycle back through Scanning"
    );
    assert_eq!(
        app.devices.selected_port().map(str::to_string),
        selected_before,
        "a second scan must not drop the already-selected port"
    );
}

#[test]
fn m_without_a_selected_port_still_lets_west_auto_detect() {
    // Symmetric with the mpremote monitor: `m` is an explicit user action,
    // and west's own auto-detection is its documented default --- ChipTUI
    // only avoids guessing for its *automatic* operations (SPEC.md §8).
    let (mut app, _root) = zephyr_app("auto");
    assert!(app.devices.selected_port().is_none());

    app.handle(key(KeyCode::Char('m')));

    assert!(
        app.device_monitor_process.is_some(),
        "west monitor without --port must still start"
    );
}
