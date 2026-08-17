//! Rendering smoke tests against ratatui's `TestBackend`.
//!
//! These need no terminal, so they run in the normal suite (`AGENTS.md`:
//! the standard tests must not require hardware or a tty). They assert what the
//! dashboard is required to show --- `SPEC.md` §11 and the first-stage
//! deliverable: directory, project type, backend, device information and
//! capabilities.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

use chiptui::app::help::{self, HelpSection};
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

fn key(code: KeyCode) -> chiptui::event::AppEvent {
    chiptui::event::AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        code,
        ratatui::crossterm::event::KeyModifiers::NONE,
    ))
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
fn a_narrow_footer_drops_whole_hints_and_keeps_the_way_out() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let frame = render(&mut app, 60, 24);
    // `TestBackend::to_string` quotes each row, so the padding *and* the
    // closing quote come off before looking at how the line ends.
    let footer = frame.lines().last().unwrap().trim_matches('"').to_string();

    assert!(footer.contains("quit"), "the way out survives:\n{footer}");
    assert!(footer.contains("help"), "and so does help:\n{footer}");
    assert!(
        footer.trim_end().ends_with("quit"),
        "the line ends on a whole hint, never mid-word:\n{footer}"
    );
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
    // workspace pane (its checklist, then the project's own files below a
    // separator --- no device filesystem to browse, but the project's
    // sources are its to show) beside the project panel, whose operation
    // buttons are dimmed until their answers exist.
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
        frame.contains("□ Zephyr Base"),
        "the checklist must ask for the installation:\n{frame}"
    );
    assert!(
        frame.contains("Project path"),
        "the project checklist must ask for the project:\n{frame}"
    );
    assert!(
        frame.contains("Project actions"),
        "the project panel must be titled as the actions pane:\n{frame}"
    );
    assert!(
        frame.contains("▶ Build"),
        "the lifecycle buttons must stay visible:\n{frame}"
    );
    assert!(
        frame.contains("─"),
        "the separator must divide checklist from buttons:\n{frame}"
    );
    assert!(
        !frame.contains("Local files:"),
        "the dual-pane browser's own title must not render for a build backend:\n{frame}"
    );
    assert!(
        !frame.contains("Device files:"),
        "no device pane may render without a filesystem:\n{frame}"
    );
    // The embedded file list is titled with the project's own name (the
    // fixture app is rooted at the temp directory).
    let project = std::env::temp_dir()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(
        frame.contains(&format!("{project}/")),
        "the workspace pane's embedded project file list must show:\n{frame}"
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
fn dashboard_shows_the_working_directory_and_backend() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let frame = render(&mut app, 120, 30);

    assert!(frame.contains("root:"), "missing root field:\n{frame}");
    assert!(frame.contains("MicroPython"), "missing backend:\n{frame}");
    assert!(
        !frame.contains("source:"),
        "the detection source no longer has a field:\n{frame}"
    );
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
fn device_pane_shows_chip_details_once_esptool_has_reported_them() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.details = DeviceDetails {
        family: Some(ChipFamily::Esp32S3),
        revision: Some("3".to_string()),
        mac: Some("24:6f:28:12:34:56".to_string()),
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

    app.overlay = Some(Overlay::Help { selected: 0 });
    let help = render(&mut app, 100, 30);
    assert!(
        help.contains("Navigation"),
        "navigation division missing:\n{help}"
    );
    assert!(
        help.contains("Commands"),
        "commands division missing:\n{help}"
    );
    assert!(
        help.contains("re-run detection, or reload the file pane"),
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
fn help_fits_one_line_per_binding_and_scrolls_under_the_cursor() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let last = help::bindings(app.view, HelpSection::Commands).len() - 1;
    app.overlay = Some(Overlay::Help { selected: last });

    // Wide enough for the whole table: every binding stays on one line ---
    // a description leaking onto a second line means the text outgrew the
    // single-window budget.
    let wide = render(&mut app, 100, 30);
    assert!(
        wide.contains("scan for devices (mpremote or USB serial)"),
        "the full description is not on one line:\n{wide}"
    );
    assert_eq!(
        wide.lines()
            .filter(|line| line.contains("scan for devices"))
            .count(),
        1,
        "the description wrapped:\n{wide}"
    );

    // Too short for the whole table: the cursor keeps the last command on
    // screen by scrolling the list.
    let short = render(&mut app, 100, 20);
    assert!(
        short.contains("q / esc / ctrl+c"),
        "the last binding was cut off vertically:\n{short}"
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

#[test]
fn long_log_entries_wrap_with_a_hanging_indent_past_the_stamp() {
    // A message wider than the pane must wrap, and every continuation row is
    // indented past the stamp so the paragraph stays tied to its timestamp.
    let mut app = app_with_backend(BackendKind::Zephyr);
    let message = format!("start {}end", "x".repeat(300));
    app.logs.info(message);

    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("start xxx"), "first row missing:\n{frame}");
    assert!(
        frame.contains(&format!("{}xxxx", " ".repeat(chiptui::logs::PREFIX_WIDTH))),
        "continuation rows must be indented past the stamp:\n{frame}"
    );
}

#[test]
fn the_log_pane_shows_a_scrollbar_once_content_overflows_it() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    for i in 0..60 {
        app.logs.info(format!("entry {i:02}"));
    }

    let frame = render(&mut app, 100, 30);
    assert!(frame.contains('┃'), "missing the scrollbar thumb:\n{frame}");
    assert!(app.logs.total_lines() >= 60);
    assert_eq!(
        app.logs.total_lines(),
        app.logs.len(),
        "short entries occupy exactly one visual line each"
    );

    // Scrolling moves the thumb: the bar reflects the visual position.
    app.focus = Focus::Logs;
    app.logs.scroll_up(usize::MAX, app.log_viewport);
    let scrolled = render(&mut app, 100, 30);
    assert_ne!(frame, scrolled, "scrolling must move the thumb");
}

#[test]
fn the_monitor_tab_shows_the_same_scrollbar_once_output_overflows_it() {
    // The Monitor tab always tails its live output, so its scrollbar is an
    // indicator: shown when the wrapped console outgrows the pane, thumb
    // pinned to the bottom.
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.log_tab = LogTab::Monitor;

    let short = render(&mut app, 100, 30);
    assert!(
        !short.contains('┃'),
        "no scrollbar before the session has output:\n{short}"
    );

    for i in 0..60 {
        app.device_monitor_output
            .push(format!("monitor line {i:02}"));
    }
    let frame = render(&mut app, 100, 30);
    assert!(frame.contains('┃'), "missing the scrollbar thumb:\n{frame}");
    assert!(
        frame.contains("monitor line 59"),
        "the console must still tail the newest output:\n{frame}"
    );
    assert!(
        !frame.contains("monitor line 00"),
        "older output scrolls out of the fixed-height pane:\n{frame}"
    );

    // A short session loses the bar again once the buffer is empty.
    app.device_monitor_output.clear();
    let cleared = render(&mut app, 100, 30);
    assert!(
        !cleared.contains('┃'),
        "the bar must leave with the output:\n{cleared}"
    );
}

#[test]
fn the_monitor_tab_scrolls_back_through_its_output() {
    // Unlike the Log pane, the Monitor tab always followed its tail and
    // could not be scrolled; now the same keys work on it. Scrolling up
    // reveals older output, live output does not shift a scrolled view, and
    // End returns to the tail.
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.focus = Focus::Logs;
    app.log_tab = LogTab::Monitor;
    for i in 0..60 {
        app.device_monitor_output
            .push(format!("monitor line {i:02}"));
    }

    let tail = render(&mut app, 100, 30);
    assert!(
        !tail.contains("monitor line 00"),
        "the tail view shows the newest output:\n{tail}"
    );

    app.handle(key(KeyCode::Home));
    let top = render(&mut app, 100, 30);
    assert!(
        top.contains("monitor line 00"),
        "Home must reach the oldest output:\n{top}"
    );
    assert!(
        top.contains('\u{2191}'),
        "a scrolled monitor must carry the lines-below indicator:\n{top}"
    );

    // New output arriving while scrolled must not move the view.
    app.device_monitor_output
        .push("monitor line 60".to_string());
    let held = render(&mut app, 100, 30);
    assert!(
        held.contains("monitor line 00"),
        "live output must not shift a scrolled monitor view:\n{held}"
    );

    app.handle(key(KeyCode::End));
    let bottom = render(&mut app, 100, 30);
    assert!(
        bottom.contains("monitor line 60"),
        "End must re-pin the tail:\n{bottom}"
    );
}

/// A Zephyr app with deterministic serial/home dirs, so the render shows the
/// workspace and build panes instead of a device picker.
fn zephyr_app() -> App {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let root = std::env::temp_dir().join(format!("chiptui-render-tabs-{}", std::process::id()));
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.maybe_scan_devices();
    app
}

#[test]
fn the_log_and_monitor_tabs_live_on_the_panes_border_row() {
    // The tab strip is not a row of its own anymore: like the Ratatui `Tabs`
    // example it sits on the pane's top border, separated by the standard
    // divider, and the pane gains the row as content.
    let mut app = zephyr_app();
    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("Log • Monitor"),
        "the tabs must share the border row with the dot divider:\n{frame}"
    );

    // Switching tabs highlights the other title on the same border.
    app.focus = Focus::Logs;
    app.handle(key(KeyCode::Right));
    assert_eq!(app.log_tab, LogTab::Monitor);
}

#[test]
fn the_monitor_tab_shows_the_running_command_with_a_spinner() {
    // While the build panel's command runs (build, clean, west update, SDK
    // list --- one slot), the Monitor tab carries its label and an animated
    // spinner, visible from the Log tab too.
    let mut app = zephyr_app();
    let command = chiptui::process::Command::new(format!(
        "{}/tests/fixtures/bin/slow",
        env!("CARGO_MANIFEST_DIR")
    ));
    let started = app
        .build
        .as_mut()
        .expect("the build panel exists for a build backend")
        .start(
            "Build",
            false,
            chiptui::build::BuildAction::Build(chiptui::backend::BuildKind::Build),
            command,
            &mut app.processes,
            &app.manager.capabilities(),
        );
    assert!(started, "the panel must accept the command");
    app.focus = Focus::Logs;
    app.log_tab = LogTab::Monitor;
    app.set_monitor_source(chiptui::app::MonitorSource::Build);

    let frame = render(&mut app, 100, 30);
    assert!(
        frame.contains("⠋ Build ("),
        "a running command must show its label, spinner and row count:\n{frame}"
    );
    assert!(
        !frame.contains('\u{2191}'),
        "no scrolled indicator while tailing:\n{frame}"
    );
}

#[test]
fn the_monitor_tab_marks_the_last_finished_command() {
    // A finished command leaves a green check behind; a failed one a red
    // cross --- both with the command's label, so the Log tab answers
    // "what did the last build do" without switching.
    let mut app = zephyr_app();
    app.focus = Focus::Logs;
    app.log_tab = LogTab::Monitor;
    app.set_monitor_source(chiptui::app::MonitorSource::Build);
    let report = |ok| chiptui::build::BuildReport {
        what: "Build",
        ok,
        duration: std::time::Duration::from_secs(3),
        at: time::OffsetDateTime::now_utc(),
    };
    app.build.as_mut().unwrap().last = Some(report(true));
    let ok = render(&mut app, 100, 30);
    assert!(
        ok.contains("✓ Build ("),
        "a finished command must be checked green with its row count:\n{ok}"
    );

    app.build.as_mut().unwrap().last = Some(report(false));
    let failed = render(&mut app, 100, 30);
    assert!(
        failed.contains("✗ Build ("),
        "a failed command must be crossed red:\n{failed}"
    );

    // Before any command ran, the title stands alone with the row count.
    app.build.as_mut().unwrap().last = None;
    let bare = render(&mut app, 100, 30);
    assert!(
        bare.contains("Build ("),
        "the source title still names the feed:\n{bare}"
    );
    assert!(
        !bare.contains("✓ Build") && !bare.contains("✗ Build"),
        "no verdict without a command:\n{bare}"
    );
}
