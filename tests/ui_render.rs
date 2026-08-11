//! Rendering smoke tests against ratatui's `TestBackend`.
//!
//! These need no terminal, so they run in the normal suite (`AGENTS.md`:
//! the standard tests must not require hardware or a tty). They assert what the
//! dashboard is required to show --- `SPEC.md` §11 and the first-stage
//! deliverable: directory, project type, confidence, backend and capabilities.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use chiptui::app::{App, Focus, Overlay};
use chiptui::backend::BackendKind;

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
fn dashboard_shows_project_backend_and_capabilities() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let frame = render(&mut app, 100, 30);

    assert!(frame.contains("ChipTUI"), "missing header:\n{frame}");
    assert!(frame.contains("Zephyr"), "missing backend name:\n{frame}");
    assert!(frame.contains("Project"), "missing project pane:\n{frame}");
    assert!(
        frame.contains("Detection"),
        "missing detection pane:\n{frame}"
    );
    assert!(
        frame.contains("Capabilities"),
        "missing capabilities pane:\n{frame}"
    );
    assert!(frame.contains("Log"), "missing log pane:\n{frame}");

    // Capabilities are rendered from the backend's declaration.
    assert!(
        frame.contains("build"),
        "missing build capability:\n{frame}"
    );
    assert!(
        frame.contains("repl"),
        "unsupported capabilities are still listed:\n{frame}"
    );
    // Destructive operations are flagged in the list itself (SPEC.md §15).
    assert!(
        frame.contains("confirm"),
        "destructive flag missing:\n{frame}"
    );

    // Footer shortcuts.
    assert!(frame.contains("quit"), "missing footer shortcuts:\n{frame}");
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
    let mut app = app_with_backend(BackendKind::Zephyr);
    let with_project_focus = render(&mut app, 100, 30);

    app.focus = Focus::Logs;
    let with_log_focus = render(&mut app, 100, 30);

    assert_ne!(
        with_project_focus, with_log_focus,
        "moving focus must change what is drawn"
    );
}
