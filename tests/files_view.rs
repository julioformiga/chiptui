//! The file browser end to end, against the fake `mpremote`.
//!
//! Covers the glue the unit tests cannot: a real process producing real bytes,
//! parsed into the device pane and compared against a real local directory.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chiptui::app::{App, Focus, Overlay, PendingEdit, ViewerSource, ViewerState};
use chiptui::backend::BackendKind;
use chiptui::browser::{Browser, PaneState, Side};
use chiptui::device::DevicePath;
use chiptui::event::AppEvent;
use chiptui::files::SyncStatus;
use chiptui::process::ProcessManager;

/// A pending edit that came from a local file (no device to re-upload to).
fn local_edit(path: PathBuf) -> PendingEdit {
    PendingEdit {
        path,
        device_target: None,
    }
}

fn fake_mpremote() -> String {
    format!("{}/tests/fixtures/bin/mpremote", env!("CARGO_MANIFEST_DIR"))
}

/// A machine with no board attached: `devs` reports only legacy UARTs.
fn fake_mpremote_no_devices() -> String {
    format!(
        "{}/tests/fixtures/bin/mpremote-no-devices",
        env!("CARGO_MANIFEST_DIR")
    )
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
fn a_lost_device_triggers_a_fresh_scan() {
    let project = Project::new("lost");
    let (mut browser, mut processes) = browser_for(&project);

    // Any path the fixture does not explicitly handle falls through to its
    // catch-all "no device found" — the same failure a real unplugged board
    // produces.
    browser.device_path = chiptui::device::DevicePath::new("/gone");
    browser.load_device(&mut processes, None, false);
    let messages = settle(&mut browser, &mut processes);

    assert!(
        messages.iter().any(|m| m.contains("disconnected")),
        "no rescan warning: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("found /dev/ttyACM0")),
        "the auto-triggered scan never completed: {messages:?}"
    );
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
fn discovering_devices_when_none_are_present() {
    let project = Project::new("no-devices");
    let mut browser = Browser::new(&project.root);
    browser.set_tool_path(fake_mpremote_no_devices());
    let mut processes = ProcessManager::new();

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
    assert!(
        devices.is_empty(),
        "only legacy UARTs were reported: {devices:?}"
    );

    // Feeding an empty scan into device state must land on Ready, not leave
    // it stuck reporting Scanning forever.
    let mut state = chiptui::device::DeviceState::new();
    state.set_scanning();
    state.set_devices(devices);
    assert_eq!(state.discovery, chiptui::device::DiscoveryState::Ready);
    assert!(state.devices().is_empty());
}

#[test]
fn the_device_picker_renders_a_helpful_empty_state() {
    // Covers the zero-devices branch of the picker overlay, reachable when a
    // scan completes with no candidates and the user still presses 'd'.
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.overlay = Some(Overlay::DevicePicker { selected: 0 });

    let frame = render(&mut app, 110, 24);
    assert!(
        frame.contains("No MicroPython device found"),
        "empty picker state not rendered:\n{frame}"
    );
}

#[test]
fn the_file_browser_panes_are_only_reachable_when_the_backend_has_a_filesystem() {
    // AGENTS.md §3: the gate is the capability, not the backend name.
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |code| AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));

    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();

    app.manager.set_override(Some(BackendKind::Zephyr));
    app.handle(key(KeyCode::Tab));
    assert_ne!(app.focus, Focus::FilesLocal, "Zephyr exposes no filesystem");
    assert!(app.browser.is_none());

    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.focus = Focus::Project;
    app.handle(key(KeyCode::Tab));
    assert_eq!(
        app.focus,
        Focus::FilesLocal,
        "MicroPython exposes a filesystem"
    );
    assert!(app.browser.is_some());
}

#[test]
fn startup_scans_for_a_device_without_opening_the_browser() {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));

    app.maybe_scan_devices();

    assert_eq!(
        app.focus,
        Focus::Project,
        "a background scan must not move focus"
    );
    assert!(
        app.browser.is_some(),
        "a scan needs somewhere to land its result"
    );
    assert_ne!(
        app.devices.discovery,
        chiptui::device::DiscoveryState::Unknown,
        "a scan must have been issued"
    );
}

#[test]
fn startup_scanning_is_gated_on_the_filesystem_capability() {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));

    app.maybe_scan_devices();

    assert!(app.browser.is_none(), "Zephyr exposes no filesystem");
    assert_eq!(
        app.devices.discovery,
        chiptui::device::DiscoveryState::Unknown
    );
}

#[test]
fn opening_the_browser_starts_on_src_when_it_exists() {
    let root =
        std::env::temp_dir().join(format!("chiptui-files-src-default-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("other.txt"), "x").unwrap();
    let expected = root.join("src");

    let mut app = App::new(&root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();

    let local_path = app.browser.as_ref().unwrap().local_path.clone();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        local_path, expected,
        "Files must open on src/, not the project root"
    );
}

#[test]
fn a_second_scan_request_does_not_rescan_an_existing_browser() {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));

    app.maybe_scan_devices();
    assert!(app.browser.is_some());
    let discovery_before = app.devices.discovery;

    // apply_picker/apply_project_setup call maybe_scan_devices() on every
    // backend change; it must not re-issue a scan once a browser exists.
    app.focus = Focus::FilesLocal;
    app.maybe_scan_devices();

    assert_eq!(app.devices.discovery, discovery_before, "no duplicate scan");
}

#[test]
fn overriding_to_a_filesystem_backend_via_the_picker_scans_for_a_device() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    assert!(
        app.browser.is_none(),
        "an unrecognized project has no filesystem to scan for yet"
    );

    // 'o' -> down to the first real backend (MicroPython) -> enter.
    let key = |code| AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
    app.handle(key(KeyCode::Char('o')));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.manager.override_kind(), Some(BackendKind::MicroPython));
    assert!(
        app.browser.is_some(),
        "picking a backend with Capability::Filesystem should scan immediately, \
         not wait for 'f'"
    );
}

#[test]
fn the_browser_state_survives_a_focus_change_but_q_quits_outright() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.focus = Focus::FilesLocal;

    // Moving focus elsewhere and back does not throw away the listing.
    app.focus = Focus::Project;
    assert!(app.browser.is_some(), "the listing is not thrown away");
    app.focus = Focus::FilesLocal;
    assert!(app.browser.is_some());

    // q/esc quits outright: there is no separate file-browser screen to step
    // back from (see `View`'s doc comment in app.rs).
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
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
    app.maybe_scan_devices();
    app.focus = Focus::FilesLocal;

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
    app.maybe_scan_devices();

    // A scan is started as soon as the backend has a filesystem; the listing
    // waits for it, so the pane reports the search rather than a bare idle
    // prompt.
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
fn dashboard_help_describes_file_browser_keys_when_a_files_pane_is_focused() {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.focus = Focus::FilesLocal;

    let shortcuts: Vec<&str> = app.shortcuts().iter().map(|(key, _)| *key).collect();
    assert!(
        shortcuts.contains(&"c"),
        "compare is offered: {shortcuts:?}"
    );
    assert!(shortcuts.contains(&"tab"));
    assert!(
        shortcuts.contains(&"o"),
        "backend override stays reachable --- files are a focus, not a \
         separate screen to leave"
    );

    app.overlay = Some(Overlay::Help);
    assert_eq!(app.shortcuts(), vec![("esc", "close")]);
}

/// Drives the app until the browser has no device command in flight ---
/// mirrors `flash_view.rs`'s `settle`, applied to the browser instead of the
/// flash panel.
fn settle_app(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while app.browser.as_ref().is_some_and(Browser::is_busy) && Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "device command never completed"
    );
}

fn key(code: ratatui::crossterm::event::KeyCode) -> AppEvent {
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn entering_a_local_text_file_opens_the_action_dialog() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("dialog-local");
    let mut app = app_in_browser(&project);
    app.browser.as_mut().unwrap().cursor_to(1); // diff.py, not a directory

    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.overlay,
        Some(Overlay::FileActions {
            side: Side::Local,
            name: "diff.py".to_string(),
            selected: 0,
        })
    );

    let frame = render(&mut app, 110, 24);
    assert!(frame.contains("Send to device"), "menu not shown:\n{frame}");
    assert!(frame.contains("View"));
    assert!(frame.contains("Edit"));
    assert!(
        !frame.contains("Download"),
        "a local file is never downloaded from itself:\n{frame}"
    );
}

#[test]
fn choosing_view_opens_the_viewer_ready_with_highlighted_content() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("dialog-view");
    let mut app = app_in_browser(&project);
    app.browser.as_mut().unwrap().cursor_to(1); // diff.py

    app.handle(key(KeyCode::Enter)); // open the dialog
    app.handle(key(KeyCode::Down)); // Send to device -> View
    app.handle(key(KeyCode::Enter)); // choose it

    assert_eq!(app.overlay, Some(Overlay::FileViewer));
    let viewer = app.viewer.as_ref().expect("viewer state opened");
    assert_eq!(
        viewer.source,
        ViewerSource::Local(project.root.join("diff.py"))
    );
    assert_eq!(
        viewer.state,
        ViewerState::Ready {
            lines: vec!["CONFIG=1".to_string()]
        }
    );

    let frame = render(&mut app, 110, 24);
    assert!(
        frame.contains("CONFIG=1"),
        "file content not shown:\n{frame}"
    );
    assert!(
        frame.contains("diff.py"),
        "title missing the name:\n{frame}"
    );
}

#[test]
fn pressing_e_in_the_viewer_queues_an_edit_and_closes_it() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("dialog-view-edit");
    let mut app = app_in_browser(&project);
    app.browser.as_mut().unwrap().cursor_to(1); // diff.py

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Down)); // View
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, Some(Overlay::FileViewer));

    app.handle(key(KeyCode::Char('e')));
    assert_eq!(
        app.overlay, None,
        "the viewer closes to hand off to $EDITOR"
    );
    assert_eq!(
        app.take_pending_edit(),
        Some(local_edit(project.root.join("diff.py")))
    );
}

#[test]
fn choosing_edit_directly_queues_an_edit_without_opening_the_viewer() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("dialog-edit");
    let mut app = app_in_browser(&project);
    app.browser.as_mut().unwrap().cursor_to(1); // diff.py

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Up)); app.handle(key(KeyCode::Up)); // wraps to Edit
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, None);
    assert!(app.viewer.is_none(), "Edit never opens the viewer");
    assert_eq!(
        app.take_pending_edit(),
        Some(local_edit(project.root.join("diff.py")))
    );
}

#[test]
fn escaping_the_viewer_does_not_queue_an_edit() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("viewer-escape");
    let mut app = app_in_browser(&project);
    app.browser.as_mut().unwrap().cursor_to(1); // diff.py

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Down)); // View
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, Some(Overlay::FileViewer));

    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
    assert!(app.viewer.is_none());
    assert_eq!(app.take_pending_edit(), None);
}

#[test]
fn entering_a_local_directory_still_descends_instead_of_opening_the_dialog() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("dialog-dir");
    let mut app = app_in_browser(&project);
    // The cursor starts on "lib/", the first entry once sorted.

    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.overlay, None,
        "a directory should descend, not open the dialog"
    );
    assert_eq!(
        app.browser.as_ref().unwrap().local_path,
        project.root.join("lib")
    );
}

#[test]
fn entering_a_binary_local_file_does_nothing() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("dialog-binary");
    std::fs::write(project.root.join("firmware.bin"), [0u8, 1, 2]).unwrap();
    let mut app = app_in_browser(&project);
    // Sorted order: lib/, diff.py, firmware.bin, local_only.py, same.py.
    app.browser.as_mut().unwrap().cursor_to(2);
    assert_eq!(
        app.browser
            .as_ref()
            .unwrap()
            .selected_name(Side::Local)
            .as_deref(),
        Some("firmware.bin")
    );

    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, None, "binary files never get the action menu");
}

#[test]
fn entering_a_device_text_file_opens_the_dialog_with_download() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("dialog-device");
    let mut app = app_in_browser(&project);
    app.focus = Focus::FilesDevice;
    let browser = app.browser.as_mut().unwrap();
    browser.focus = Side::Device;
    browser.cursor_to(1); // device_only.py --- a file, not a directory

    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.overlay,
        Some(Overlay::FileActions {
            side: Side::Device,
            name: "device_only.py".to_string(),
            selected: 0,
        })
    );

    let frame = render(&mut app, 110, 24);
    assert!(frame.contains("Download"), "menu not shown:\n{frame}");
    assert!(frame.contains("View"));
    assert!(frame.contains("Edit"));
    assert!(
        !frame.contains("Send to device"),
        "a device file is never sent to the device it is already on:\n{frame}"
    );
}

#[test]
fn choosing_view_on_a_device_file_loads_its_content_asynchronously() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("device-view");
    let mut app = app_in_browser(&project);
    app.focus = Focus::FilesDevice;
    let browser = app.browser.as_mut().unwrap();
    browser.focus = Side::Device;
    browser.cursor_to(1); // device_only.py

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Down)); // Download -> View
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, Some(Overlay::FileViewer));
    assert_eq!(
        app.viewer.as_ref().unwrap().state,
        ViewerState::Loading,
        "the cat has not returned yet"
    );

    settle_app(&mut app);

    let viewer = app.viewer.as_ref().expect("still open");
    assert_eq!(
        viewer.source,
        ViewerSource::Device(chiptui::device::DevicePath::new("/device_only.py"))
    );
    assert_eq!(
        viewer.state,
        ViewerState::Ready {
            lines: vec!["device content".to_string()]
        }
    );
}

#[test]
fn choosing_download_on_a_device_file_writes_it_locally() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("device-download");
    let mut app = app_in_browser(&project);
    app.focus = Focus::FilesDevice;
    let browser = app.browser.as_mut().unwrap();
    browser.focus = Side::Device;
    browser.cursor_to(1); // device_only.py

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Enter)); // Download is already selected

    assert_eq!(app.overlay, None, "download is fire-and-forget, no overlay");
    settle_app(&mut app);

    let downloaded = project.root.join("device_only.py");
    assert!(downloaded.exists(), "the file was not written locally");
    assert_eq!(
        std::fs::read_to_string(&downloaded).unwrap(),
        "device content\n"
    );
    assert!(
        app.take_pending_edit().is_none(),
        "plain download never edits"
    );
}

#[test]
fn choosing_edit_on_a_device_file_downloads_it_to_a_temp_file_then_queues_an_edit() {
    // The whole point: editing a device file must be provable on the device
    // first, so the download for $EDITOR goes to a scratch temp file, never
    // into the project tree. `Download` is the separate, explicit step for
    // bringing a confirmed-good result into the project.
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("device-edit");
    let mut app = app_in_browser(&project);
    app.focus = Focus::FilesDevice;
    let browser = app.browser.as_mut().unwrap();
    browser.focus = Side::Device;
    browser.cursor_to(1); // device_only.py

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Up)); app.handle(key(KeyCode::Up)); // wraps to Edit
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, None);
    settle_app(&mut app);

    let pending = app
        .take_pending_edit()
        .expect("editing a device file should queue $EDITOR");
    assert_eq!(
        pending.device_target,
        Some(DevicePath::new("/device_only.py")),
        "remembers where to re-upload it"
    );
    assert!(
        pending.path.starts_with(std::env::temp_dir()),
        "an edit download must land in a scratch temp file: {}",
        pending.path.display()
    );
    assert!(
        !pending.path.starts_with(&project.root),
        "an edit download must never touch the project tree: {}",
        pending.path.display()
    );
    assert_eq!(
        std::fs::read_to_string(&pending.path).unwrap(),
        "device content\n"
    );

    assert!(
        !project.root.join("device_only.py").exists(),
        "the project tree must stay untouched until an explicit Download"
    );
}

#[test]
fn pressing_e_in_a_device_viewer_also_downloads_to_a_temp_file() {
    // The viewer's `e` key is a second path into the same edit chain
    // (`FileAction::Edit` on the dialog is the other) --- it must get the
    // same "never touch the project tree" treatment.
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("device-view-edit");
    let mut app = app_in_browser(&project);
    app.focus = Focus::FilesDevice;
    let browser = app.browser.as_mut().unwrap();
    browser.focus = Side::Device;
    browser.cursor_to(1); // device_only.py

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Down)); // Download -> View
    app.handle(key(KeyCode::Enter));
    settle_app(&mut app); // let the cat land so the viewer is Ready

    app.handle(key(KeyCode::Char('e')));
    assert_eq!(
        app.overlay, None,
        "the viewer closes to hand off to $EDITOR"
    );
    settle_app(&mut app);

    let pending = app
        .take_pending_edit()
        .expect("the viewer's e key should queue $EDITOR");
    assert!(pending.path.starts_with(std::env::temp_dir()));
    assert!(!pending.path.starts_with(&project.root));
    assert!(!project.root.join("device_only.py").exists());
}

#[test]
fn a_clean_editor_exit_reuploads_and_offers_a_restart() {
    // Simulates what `main.rs`'s `run_editor` does once `$EDITOR` exits
    // successfully on a device-sourced edit: it is the one place that knows
    // the editor actually ran, so this is exercised at the level `App`
    // exposes for it (`request_device_reupload`) rather than through a real
    // terminal, which the test suite has none of.
    let project = Project::new("reupload-restart");
    let mut app = app_in_browser(&project);

    let local_path = project.root.join("device_only.py");
    std::fs::write(&local_path, "edited\n").unwrap();
    app.request_device_reupload(local_path, DevicePath::new("/device_only.py"));

    assert_eq!(
        app.overlay, None,
        "the prompt only appears once the upload actually lands"
    );
    settle_app(&mut app);

    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmRestartDevice { confirm: false }),
        "a successful re-upload after editing should offer a restart"
    );
}

#[test]
fn an_ordinary_send_to_device_never_offers_a_restart() {
    // The restart prompt is specific to the edit-then-reupload chain, not
    // every upload --- `FileAction::SendToDevice` from the dialog must stay
    // fire-and-forget, matching `sending_a_local_file_to_the_device_uploads_it`.
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("send-no-restart");
    let mut app = app_in_browser(&project);
    app.browser.as_mut().unwrap().cursor_to(2); // local_only.py

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Enter)); // Send to device
    app.handle(key(KeyCode::Char('y'))); // Confirm upload

    settle_app(&mut app);
    assert_eq!(app.overlay, None, "an ordinary upload never prompts");
}

#[test]
fn pressing_y_on_the_restart_prompt_queues_a_soft_reset() {
    let project = Project::new("restart-yes");
    let mut app = app_in_browser(&project);
    app.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });

    app.handle(key(ratatui::crossterm::event::KeyCode::Char('y')));
    assert_eq!(app.overlay, None);
    settle_app(&mut app);

    assert!(
        app.logs
            .visible(20)
            .any(|entry| entry.message.contains("reset")),
        "the soft-reset was not logged"
    );
}

#[test]
fn enter_on_the_restart_prompt_defaults_to_no() {
    let project = Project::new("restart-default-no");
    let mut app = app_in_browser(&project);
    app.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });

    // Enter is the reflex keypress; it must not be the one that restarts.
    app.handle(key(ratatui::crossterm::event::KeyCode::Enter));
    assert_eq!(app.overlay, None);
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "declining must not start a reset"
    );
}

#[test]
fn arrow_keys_toggle_the_restart_prompt_s_highlighted_button() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("restart-toggle");
    let mut app = app_in_browser(&project);
    app.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });

    app.handle(key(KeyCode::Right));
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmRestartDevice { confirm: true }),
        "Right moves the highlight onto Yes"
    );

    app.handle(key(KeyCode::Left));
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmRestartDevice { confirm: false }),
        "Left moves it back onto No"
    );
}

#[test]
fn confirming_the_highlighted_yes_button_restarts() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("restart-highlighted-yes");
    let mut app = app_in_browser(&project);
    app.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });

    app.handle(key(KeyCode::Right)); // highlight Yes
    app.handle(key(KeyCode::Enter)); // confirm whichever is highlighted

    assert_eq!(app.overlay, None);
    settle_app(&mut app);
    assert!(
        app.logs
            .visible(20)
            .any(|entry| entry.message.contains("reset")),
        "Enter should restart once Yes is the highlighted button"
    );
}

#[test]
fn esc_and_n_also_decline_the_restart_prompt() {
    use ratatui::crossterm::event::KeyCode;

    for code in [KeyCode::Esc, KeyCode::Char('n')] {
        let project = Project::new("restart-decline");
        let mut app = app_in_browser(&project);
        app.overlay = Some(Overlay::ConfirmRestartDevice { confirm: false });

        app.handle(key(code));
        assert_eq!(app.overlay, None);
        assert!(!app.browser.as_ref().unwrap().is_busy());
    }
}

#[test]
fn sending_a_local_file_to_the_device_uploads_it() {
    use ratatui::crossterm::event::KeyCode;

    let project = Project::new("send-to-device");
    let mut app = app_in_browser(&project);
    // Sorted order: lib/, diff.py, local_only.py, same.py --- the fake
    // mpremote only accepts an upload targeting :/local_only.py.
    app.browser.as_mut().unwrap().cursor_to(2);
    assert_eq!(
        app.browser
            .as_ref()
            .unwrap()
            .selected_name(Side::Local)
            .as_deref(),
        Some("local_only.py")
    );

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Enter)); // Send to device
    app.handle(key(KeyCode::Char('y'))); // Confirm upload

    assert_eq!(app.overlay, None);
    settle_app(&mut app);
    assert!(
        app.logs
            .visible(20)
            .any(|entry| entry.message.contains("uploaded")),
        "no success notice logged"
    );
}
