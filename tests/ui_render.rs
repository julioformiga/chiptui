//! Rendering smoke tests against ratatui's `TestBackend`.
//!
//! These need no terminal, so they run in the normal suite (`AGENTS.md`:
//! the standard tests must not require hardware or a tty). They assert what the
//! dashboard is required to show --- `SPEC.md` §11 and the first-stage
//! deliverable: directory, project type, backend, device information and
//! capabilities.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use chiptui::app::{App, Focus, LogTab, Overlay};
use chiptui::backend::BackendKind;
use chiptui::backend::micropython::esptool::{ChipFamily, DeviceDetails};
use chiptui::flash::FlashPanel;

/// Renders the dashboard at `width`x`height` and returns it as plain text.
fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| chiptui::ui::draw(frame, app))
        .expect("draw succeeds");
    terminal.backend().to_string()
}

/// An app whose detection has been forced to a known backend, so the assertions
/// do not depend on the directory the tests happen to run in.
fn app_with_backend(kind: BackendKind) -> App {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(kind));
    app
}

#[test]
fn dashboard_shows_project_device_and_log_panes() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let frame = render(&mut app, 140, 30);

    assert!(frame.contains("ChipTUI"), "missing header:\n{frame}");
    assert!(frame.contains("Zephyr"), "missing backend name:\n{frame}");
    assert!(frame.contains("Project"), "missing project pane:\n{frame}");
    assert!(frame.contains("Device"), "missing device pane:\n{frame}");
    assert!(frame.contains("Log"), "missing log pane:\n{frame}");

    // Row 2 before any browser exists (bootstrap only, no
    // maybe_scan_devices yet): the full-width placeholder.
    assert!(
        frame.contains("file browsing not implemented"),
        "missing the no-browser placeholder:\n{frame}"
    );

    // Footer shortcuts.
    assert!(frame.contains("quit"), "missing footer shortcuts:\n{frame}");
}

#[test]
fn zephyr_shows_the_workspace_and_build_panes_in_row_two() {
    // Once startup has run, row 2 belongs entirely to the build backend: the
    // workspace pane (no filesystem to browse, no file listing --- that is
    // the editor's job) beside the build panel quoting the literal `west`
    // commands.
    let mut app = app_with_backend(BackendKind::Zephyr);
    // Empty fixture /dev and an isolated home keep the render deterministic
    // (a machine with several USB ports would open the device picker; one
    // with ~/zephyrproject would resolve the workspace).
    let root = std::env::temp_dir().join(format!("chiptui-render-zs-{}", std::process::id()));
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.maybe_scan_devices();
    let frame = render(&mut app, 100, 30);

    assert!(
        frame.contains("Workspace"),
        "missing the workspace pane:\n{frame}"
    );
    assert!(
        frame.contains("no west workspace found"),
        "an unresolved pane must explain itself:\n{frame}"
    );
    assert!(frame.contains("Build"), "missing the build panel:\n{frame}");
    assert!(
        frame.contains("west build"),
        "the panel must quote the literal commands:\n{frame}"
    );
    assert!(
        !frame.contains("Local files:"),
        "no file pane may render for a build backend:\n{frame}"
    );
    assert!(
        !frame.contains("Device files:"),
        "no device pane may render without a filesystem:\n{frame}"
    );
}

#[test]
fn dashboard_shows_the_file_browser_in_row_two_for_a_filesystem_backend() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.maybe_scan_devices();
    let frame = render(&mut app, 100, 30);

    assert!(
        frame.contains("Local"),
        "missing the local file pane:\n{frame}"
    );
    assert!(
        !frame.contains("file browsing not implemented"),
        "MicroPython has a filesystem, no placeholder expected:\n{frame}"
    );
}

#[test]
fn row_three_shows_a_log_monitor_tab_strip() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let frame = render(&mut app, 100, 30);

    assert!(frame.contains("Log"), "missing the Log tab:\n{frame}");
    assert!(
        frame.contains("Monitor"),
        "missing the Monitor tab:\n{frame}"
    );
}

#[test]
fn switching_to_the_monitor_tab_changes_row_three() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.focus = Focus::Logs;
    let on_log = render(&mut app, 100, 30);

    app.log_tab = LogTab::Monitor;
    let on_monitor = render(&mut app, 100, 30);

    assert_ne!(
        on_log, on_monitor,
        "switching row 3's tab must change what is drawn"
    );
    assert!(
        on_monitor.contains("not connected"),
        "Monitor tab should show its placeholder body:\n{on_monitor}"
    );
}

#[test]
fn dashboard_shows_the_working_directory_and_detection_source() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let frame = render(&mut app, 120, 30);

    assert!(frame.contains("root:"), "missing root field:\n{frame}");
    assert!(
        frame.contains("manual override"),
        "missing detection source:\n{frame}"
    );
    assert!(frame.contains("MicroPython"), "missing backend:\n{frame}");
}

#[test]
fn device_pane_prompts_to_open_flash_before_anything_has_been_queried() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let frame = render(&mut app, 100, 30);

    assert!(
        frame.contains("press 'x'"),
        "missing hint to open the flash view:\n{frame}"
    );
}

#[test]
fn device_pane_has_nothing_to_show_without_a_flash_capable_backend() {
    // No override and an empty directory: detection concludes `Unknown`, so
    // `selected_kind()` is `None` and capabilities() is empty --- neither
    // currently-registered backend lacks Flash on its own, so this is the
    // only way to exercise the "no capability" branch.
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    let frame = render(&mut app, 100, 30);

    assert!(
        frame.contains("no device information"),
        "expected a capability-appropriate message:\n{frame}"
    );
}

#[test]
fn device_pane_shows_chip_and_flash_details_once_esptool_has_reported_them() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.details = DeviceDetails {
        family: Some(ChipFamily::Esp32S3),
        revision: Some("3".to_string()),
        mac: Some("24:6f:28:12:34:56".to_string()),
        flash_size: Some("4MB".to_string()),
        ..DeviceDetails::default()
    };
    app.flash = Some(flash);

    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("Device info"),
        "missing pane title:\n{frame}"
    );
    assert!(frame.contains("ESP32-S3"), "missing chip family:\n{frame}");
    assert!(frame.contains("revision 3"), "missing revision:\n{frame}");
    assert!(
        frame.contains("24:6f:28:12:34:56"),
        "missing MAC address:\n{frame}"
    );
    assert!(frame.contains("4MB"), "missing flash size:\n{frame}");
}

#[test]
fn a_too_small_terminal_degrades_instead_of_panicking() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let frame = render(&mut app, 24, 6);
    assert!(
        frame.contains("too small"),
        "expected a size warning:\n{frame}"
    );
}

#[test]
fn rendering_survives_a_wide_range_of_sizes() {
    // Stands in for interactive resizing: every size must draw without panicking.
    let mut app = app_with_backend(BackendKind::Zephyr);
    for (width, height) in [
        (60, 14),
        (80, 24),
        (100, 30),
        (200, 60),
        (61, 15),
        (250, 15),
    ] {
        let frame = render(&mut app, width, height);
        assert!(!frame.is_empty(), "empty frame at {width}x{height}");
    }
}

#[test]
fn overlays_draw_above_the_dashboard() {
    let mut app = app_with_backend(BackendKind::Zephyr);

    app.overlay = Some(Overlay::Help);
    let help = render(&mut app, 100, 30);
    assert!(help.contains("Keyboard"), "help overlay missing:\n{help}");
    assert!(
        help.contains("re-run project detection"),
        "help body missing:\n{help}"
    );

    app.overlay = Some(Overlay::BackendPicker { selected: 0 });
    let picker = render(&mut app, 100, 30);
    assert!(picker.contains("Automatic"), "picker missing:\n{picker}");
    assert!(
        picker.contains("MicroPython"),
        "picker options missing:\n{picker}"
    );

    app.overlay = Some(Overlay::ProjectSetup { selected: 0 });
    let setup = render(&mut app, 100, 30);
    assert!(
        setup.contains("New project"),
        "project setup overlay missing:\n{setup}"
    );
    assert!(
        setup.contains("MicroPython") && setup.contains("Zephyr"),
        "project setup options missing:\n{setup}"
    );
    assert!(
        !setup.contains("Automatic"),
        "detection already failed to conclude one, so there is nothing to fall back to:\n{setup}"
    );
}

#[test]
fn the_renderer_publishes_the_log_viewport_height() {
    // The log pane's height drives page-scrolling, so it must reflect the frame.
    let mut app = app_with_backend(BackendKind::Zephyr);

    render(&mut app, 100, 30);
    let tall = app.log_viewport;
    render(&mut app, 100, 16);
    let short = app.log_viewport;

    assert!(
        tall > short,
        "viewport did not shrink with the frame: {tall} vs {short}"
    );
    assert!(short >= 1);
}

#[test]
fn focus_is_visible_in_the_rendered_frame() {
    // The Project/Device info row is never focused, so the visible focus
    // change is between the file-browser panes and the log pane. MicroPython
    // declares a filesystem, giving both something to render.
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.maybe_scan_devices();
    app.focus = Focus::FilesLocal;
    let with_files_focus = render(&mut app, 100, 30);

    app.focus = Focus::Logs;
    let with_log_focus = render(&mut app, 100, 30);

    assert_ne!(
        with_files_focus, with_log_focus,
        "moving focus must change what is drawn"
    );
}
