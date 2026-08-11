//! The file browser end to end, against the fake `mpremote`.
//!
//! Covers the glue the unit tests cannot: a real process producing real bytes,
//! parsed into the device pane and compared against a real local directory.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chiptui::app::{App, Overlay, View};
use chiptui::backend::BackendKind;
use chiptui::browser::{Browser, PaneState, Side};
use chiptui::event::AppEvent;
use chiptui::files::SyncStatus;
use chiptui::process::ProcessManager;

fn fake_mpremote() -> String {
    format!("{}/tests/fixtures/bin/mpremote", env!("CARGO_MANIFEST_DIR"))
}

/// A local project laid out to exercise every comparison outcome against the
/// device listing the fake returns.
struct Project {
    root: PathBuf,
}

impl Project {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("chiptui-files-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("lib")).unwrap();
        // Byte-identical to the device copy: same size, and the fake returns
        // this file's real digest.
        std::fs::write(root.join("same.py"), "print('hi')\n").unwrap();
        // Same length as the device copy but different contents.
        std::fs::write(root.join("diff.py"), "CONFIG=1\n").unwrap();
        std::fs::write(root.join("local_only.py"), "x = 1\n").unwrap();
        std::fs::write(root.join(".hidden"), "secret").unwrap();
        Self { root }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn browser_for(project: &Project) -> (Browser, ProcessManager) {
    let mut browser = Browser::new(&project.root);
    browser.set_tool_path(fake_mpremote());
    (browser, ProcessManager::new())
}

/// Drives the browser until no device command is outstanding.
fn settle(browser: &mut Browser, processes: &mut ProcessManager) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut messages = Vec::new();

    while browser.is_busy() && Instant::now() < deadline {
        for event in processes.drain() {
            let update = browser.on_process(&event, processes, None);
            messages.extend(update.notices.into_iter().map(|(_, text)| text));
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(!browser.is_busy(), "device command never completed");
    messages
}

#[test]
fn lists_the_device_root() {
    let project = Project::new("root");
    let (mut browser, mut processes) = browser_for(&project);

    browser.load_device(&mut processes, None, false);
    assert_eq!(
        browser.device_state,
        PaneState::Loading,
        "the pane stays usable while loading"
    );
    settle(&mut browser, &mut processes);

    assert_eq!(browser.device_state, PaneState::Ready);
    let names: Vec<&str> = browser
        .visible_device()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["lib", "device_only.py", "diff.py", "same.py"],
        "directories first, then files alphabetically"
    );
}

#[test]
fn compares_the_two_panes() {
    let project = Project::new("compare");
    let (mut browser, mut processes) = browser_for(&project);

    browser.load_device(&mut processes, None, false);
    settle(&mut browser, &mut processes);

    let statuses = browser.statuses();
    // Same length, contents not yet checked --- deliberately not "identical".
    assert_eq!(statuses["same.py"], SyncStatus::SameSize);
    assert_eq!(
        statuses["diff.py"],
        SyncStatus::SameSize,
        "9 bytes on both sides"
    );
    assert_eq!(statuses["local_only.py"], SyncStatus::LocalOnly);
    assert_eq!(statuses["device_only.py"], SyncStatus::DeviceOnly);
    assert_eq!(statuses["lib"], SyncStatus::Directory);
    // Hidden by default, so it is not part of the comparison.
    assert!(!statuses.contains_key(".hidden"));
}

#[test]
fn hashing_settles_what_sizes_cannot() {
    let project = Project::new("hash");
    let (mut browser, mut processes) = browser_for(&project);

    browser.load_device(&mut processes, None, false);
    settle(&mut browser, &mut processes);

    // Local pane order: lib/, diff.py, local_only.py, same.py.
    // same.py: identical bytes.
    browser.cursor_to(3);
    assert_eq!(
        browser.selected_name(Side::Local).as_deref(),
        Some("same.py")
    );
    browser.verify_selected(&mut processes, None);
    settle(&mut browser, &mut processes);
    assert_eq!(browser.statuses()["same.py"], SyncStatus::Identical);

    // diff.py: same length, different contents --- invisible to a size check.
    browser.cursor_to(1);
    assert_eq!(
        browser.selected_name(Side::Local).as_deref(),
        Some("diff.py")
    );
    browser.verify_selected(&mut processes, None);
    settle(&mut browser, &mut processes);
    assert_eq!(browser.statuses()["diff.py"], SyncStatus::Differs);
}

#[test]
fn navigates_into_a_device_directory() {
    let project = Project::new("nav");
    let (mut browser, mut processes) = browser_for(&project);

    browser.load_device(&mut processes, None, false);
    settle(&mut browser, &mut processes);

    browser.focus = Side::Device;
    browser.cursor_to(0); // lib/
    browser.enter(&mut processes, None);
    settle(&mut browser, &mut processes);

    assert_eq!(browser.device_path.as_str(), "/lib");
    let names: Vec<&str> = browser
        .visible_device()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["simple.py"]);

    browser.ascend(&mut processes, None);
    settle(&mut browser, &mut processes);
    assert_eq!(browser.device_path.as_str(), "/");
    assert!(
        !browser.visible_device().is_empty(),
        "the parent listing was cached"
    );
}

#[test]
fn a_failing_listing_leaves_the_pane_usable() {
    let project = Project::new("fail");
    let (mut browser, mut processes) = browser_for(&project);

    browser.device_path = chiptui::device::DevicePath::new("/missing");
    browser.load_device(&mut processes, None, false);
    let messages = settle(&mut browser, &mut processes);

    match &browser.device_state {
        PaneState::Failed(error) => assert!(
            error.contains("does not exist"),
            "raw stderr should be translated: {error}"
        ),
        other => panic!("expected a failed pane, got {other:?}"),
    }
    assert!(messages.iter().any(|m| m.contains("does not exist")));
    // The local pane is untouched by a device failure.
    assert!(!browser.visible_local().is_empty());
}

#[test]
fn discovers_devices_and_ignores_legacy_ports() {
    let project = Project::new("devs");
    let (mut browser, mut processes) = browser_for(&project);

    browser.scan_devices(&mut processes, None);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut scan = None;
    while browser.is_busy() && Instant::now() < deadline {
        for event in processes.drain() {
            let update = browser.on_process(&event, &mut processes, None);
            if let Some(result) = update.device_scan {
                scan = Some(result);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let devices = scan.expect("a scan result").expect("the scan succeeded");
    assert_eq!(devices.len(), 1, "the 0000:0000 UARTs are not candidates");
    assert_eq!(devices[0].port, "/dev/ttyACM0");
}

#[test]
fn the_browser_is_gated_on_the_filesystem_capability() {
    // AGENTS.md §3: the gate is the capability, not the backend name.
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();

    app.manager.set_override(Some(BackendKind::Zephyr));
    app.open_files();
    assert_eq!(app.view, View::Dashboard, "Zephyr exposes no filesystem");
    assert!(app.browser.is_none());

    app.manager.set_override(Some(BackendKind::MicroPython));
    app.open_files();
    assert_eq!(app.view, View::Files);
    assert!(app.browser.is_some());
}

#[test]
fn leaving_the_browser_keeps_its_state() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.open_files();

    // `q` returns to the dashboard rather than quitting.
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.view, View::Dashboard);
    assert!(!app.should_quit());
    assert!(app.browser.is_some(), "the listing is not thrown away");

    // Re-opening does not rebuild it.
    app.open_files();
    assert_eq!(app.view, View::Files);

    // Ctrl+C still quits from the browser.
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert!(app.should_quit());
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

/// An app sitting in the file browser, with the device pane already loaded.
fn app_in_browser(project: &Project) -> App {
    let mut app = App::new(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.open_files();

    let mut browser = app.browser.take().unwrap();
    browser.set_tool_path(fake_mpremote());
    browser.load_device(&mut app.processes, None, true);
    settle(&mut browser, &mut app.processes);
    app.browser = Some(browser);
    app
}

#[test]
fn the_browser_renders_both_panes_with_comparison_markers() {
    let project = Project::new("render");
    let mut app = app_in_browser(&project);
    let frame = render(&mut app, 110, 24);

    assert!(frame.contains("Local"), "missing local pane:\n{frame}");
    assert!(frame.contains("Device"), "missing device pane:\n{frame}");
    assert!(
        frame.contains("local_only.py"),
        "missing a local entry:\n{frame}"
    );
    assert!(
        frame.contains("device_only.py"),
        "missing a device entry:\n{frame}"
    );

    // The comparison markers and their legend.
    assert!(
        frame.contains(SyncStatus::LocalOnly.marker()),
        "no → marker:\n{frame}"
    );
    assert!(
        frame.contains(SyncStatus::DeviceOnly.marker()),
        "no ← marker:\n{frame}"
    );
    assert!(
        frame.contains("device only"),
        "missing the legend:\n{frame}"
    );

    // Browser keys, not dashboard keys.
    assert!(
        frame.contains("compare"),
        "missing footer shortcuts:\n{frame}"
    );
    assert!(frame.contains("DIR"), "directories are marked:\n{frame}");
}

#[test]
fn a_pending_listing_renders_a_spinner_not_a_frozen_pane() {
    let project = Project::new("spinner");
    let mut app = App::new(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.open_files();

    // Opening the browser starts a device scan; the listing waits for it, so
    // the pane reports the search rather than a bare idle prompt.
    let frame = render(&mut app, 110, 24);
    assert!(
        frame.contains("searching for a device"),
        "no progress indication:\n{frame}"
    );
    // The whole point of doing this off the event loop: the local side is live.
    assert!(
        frame.contains("local_only.py"),
        "the local pane is blocked:\n{frame}"
    );
}

#[test]
fn the_device_picker_lists_only_real_boards() {
    let project = Project::new("picker");
    let mut app = app_in_browser(&project);
    app.devices.set_devices(vec![
        chiptui::device::DeviceInfo {
            port: "/dev/ttyACM0".into(),
            serial: None,
            vid_pid: "2e8a:0005".into(),
            description: "MicroPython Board".into(),
        },
        chiptui::device::DeviceInfo {
            port: "/dev/ttyUSB0".into(),
            serial: None,
            vid_pid: "10c4:ea60".into(),
            description: "CP2102".into(),
        },
    ]);
    app.overlay = Some(Overlay::DevicePicker { selected: 0 });

    let frame = render(&mut app, 110, 24);
    assert!(
        frame.contains("/dev/ttyACM0"),
        "picker missing a device:\n{frame}"
    );
    assert!(
        frame.contains("2e8a:0005"),
        "picker missing the vid:pid:\n{frame}"
    );
    // Several devices and no selection: the header must not claim one.
    assert!(
        frame.contains("none selected"),
        "header overstates state:\n{frame}"
    );
}

#[test]
fn the_browser_survives_a_range_of_sizes() {
    let project = Project::new("sizes");
    let mut app = app_in_browser(&project);
    for (width, height) in [(60, 14), (80, 24), (110, 24), (200, 50), (61, 15)] {
        assert!(!render(&mut app, width, height).is_empty());
    }
}

#[test]
fn help_in_the_browser_describes_browser_keys() {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.open_files();

    let shortcuts: Vec<&str> = app.shortcuts().iter().map(|(key, _)| *key).collect();
    assert!(
        shortcuts.contains(&"c"),
        "compare is offered: {shortcuts:?}"
    );
    assert!(shortcuts.contains(&"tab"));
    assert!(
        !shortcuts.contains(&"o"),
        "backend override is a dashboard key"
    );

    app.overlay = Some(Overlay::Help);
    assert_eq!(app.shortcuts(), vec![("esc", "close")]);
}
