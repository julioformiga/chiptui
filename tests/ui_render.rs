//! Rendering smoke tests against ratatui's `TestBackend`.
//!
//! These need no terminal, so they run in the normal suite (`AGENTS.md`:
//! the standard tests must not require hardware or a tty). They assert what the
//! dashboard is required to show --- `SPEC.md` §11 and the first-stage
//! deliverable: directory, project type, backend, device information and
//! capabilities.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

use chiptui::app::help::{self, HelpSection};
use chiptui::app::{App, Focus, LogTab, Overlay};
use chiptui::backend::BackendKind;
use chiptui::backend::micropython::esptool::{ChipFamily, DeviceDetails};
use chiptui::firmware_id::FirmwareVerdict;
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

fn ctrl(c: char) -> chiptui::event::AppEvent {
    chiptui::event::AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        KeyCode::Char(c),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    ))
}

/// A home directory that does not exist, unique per call. `bootstrap`
/// reads the user config out of `$HOME`, so without this the frames
/// assert against whatever the developer happens to have configured ---
/// the theme picker's `(active)` row moves to `Auto` for anyone whose
/// own `[ui] theme` is `auto`, and a test that ever answers a prompt
/// would write into the real config (`CLAUDE.md`).
fn scratch_home() -> PathBuf {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "chiptui-ui-render-home-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// An app whose detection has been forced to a known backend, so the assertions
/// do not depend on the directory the tests happen to run in --- nor on the
/// configuration of the machine running them ([`scratch_home`]).
fn app_with_backend(kind: BackendKind) -> App {
    let mut app = App::new(std::env::temp_dir());
    app.set_home_dir(scratch_home());
    app.bootstrap();
    app.manager.set_override(Some(kind));
    app
}

#[test]
fn a_narrow_footer_drops_whole_hints_and_keeps_the_way_out() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let frame = render(&mut app, 80, 32);
    // `TestBackend::to_string` quotes each row, so the padding *and* the
    // closing quote come off before looking at how the line ends.
    let footer = frame.lines().last().unwrap().trim_matches('"').to_string();

    assert!(
        footer.contains("help"),
        "the way to the rest survives:\n{footer}"
    );
    assert!(
        footer.trim_end().ends_with("help"),
        "the line ends on a whole hint, never mid-word:\n{footer}"
    );
}

/// The button glyphs come from the session's icon set: plain Unicode by
/// default, Nerd Font codepoints only when `[ui] icons = "nerd"` opted in
/// (the render test drives the same seam `set_home_dir`-style tests use,
/// so both renderings are exercised without a config file).
#[test]
fn the_icon_set_swaps_the_action_buttons_glyphs() {
    let mut app = header_fixture("icons");
    let unicode = render(&mut app, 100, 32);
    assert!(
        unicode.contains('⇩'),
        "the Unicode set is the default (Install Zephyr's glyph):\n{unicode}"
    );

    app.set_icon_set(chiptui::icons::IconSet::Nerd);
    let nerd = render(&mut app, 100, 32);
    assert!(
        nerd.contains('\u{F019}'),
        "the Nerd set replaces the pane's glyphs:\n{nerd}"
    );
    assert!(
        !nerd.contains('⇩'),
        "no pane glyph survives from the default set:\n{nerd}"
    );
}

/// `none` strips the glyphs everywhere the icon set reaches: the buttons
/// keep their labels, centered, and the file rows their names --- only the
/// decoration is gone. The pane's geometry is untouched (the glyph never
/// had a row of its own).
#[test]
fn the_none_icon_set_leaves_labels_without_glyphs() {
    let mut app = header_fixture("icons-none");
    let frame = render(&mut app, 100, 32);

    app.set_icon_set(chiptui::icons::IconSet::None);
    let bare = render(&mut app, 100, 32);
    for glyph in ['▶', '×', '⟳', '⇧', '✎', '⇩'] {
        assert!(
            !bare.contains(glyph),
            "the {glyph} button glyph survived the none set:\n{bare}"
        );
    }
    for label in ["Install Zephyr", "Menuconfig", "Build", "Flash"] {
        assert!(
            bare.contains(label),
            "the `{label}` label survives without its glyph:\n{bare}"
        );
    }
    assert!(
        frame.contains('⇩'),
        "sanity: the default set draws the glyph this test strips\n{frame}"
    );
}

/// Pane titles and tab strips carry the icon set's decoration glyphs: the
/// environment checklist, the chip-identity pane, the actions pane, and
/// row 3's tabs --- each from the session's set, hidden whole by `none`
/// (they are decoration, `shows_decorations`).
#[test]
fn pane_titles_and_tab_strips_carry_the_icon_sets_glyphs() {
    let mut app = header_fixture("icons-panes");
    let frame = render(&mut app, 100, 32);

    assert!(
        frame.contains("☰ Environment"),
        "the environment pane's title:\n{frame}"
    );
    assert!(
        frame.contains("◆ Device Info"),
        "the chip-identity pane reuses the chip glyph:\n{frame}"
    );
    assert!(
        frame.contains("↯ Actions"),
        "the build pane's title:\n{frame}"
    );
    assert!(
        frame.contains("▤ Log • ◉ Monitor • › Terminal"),
        "row 3's tab strip:\n{frame}"
    );

    app.set_icon_set(chiptui::icons::IconSet::None);
    let bare = render(&mut app, 100, 32);
    for glyph in ['☰', '◆', '↯', '▤', '◉', '›'] {
        assert!(
            !bare.contains(glyph),
            "the {glyph} pane/tab glyph survived the none set:\n{bare}"
        );
    }
    for title in [
        "Environment",
        "Device Info",
        "Actions",
        "Log • Monitor • Terminal",
    ] {
        assert!(
            bare.contains(title),
            "the `{title}` title survives without its glyph:\n{bare}"
        );
    }
}

#[test]
fn dashboard_shows_project_device_and_log_panes() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let frame = render(&mut app, 140, 32);

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
    assert!(frame.contains("help"), "missing footer shortcuts:\n{frame}");
}

/// A Zephyr fixture with a buildable project root and one connected board.
fn header_fixture(tag: &str) -> App {
    header_fixture_named(tag, "esp32c3_oled")
}

/// The same fixture with the project's folder name chosen by the caller ---
/// the header's centre section is sized against it, so a test about
/// ellipsizing needs a name too long for the frame it renders at.
fn header_fixture_named(tag: &str, project: &str) -> App {
    use chiptui::device::DeviceInfo;

    let base = std::env::temp_dir().join(format!("chiptui-header-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join(project);
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(base.join("home")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();

    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(base.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.maybe_scan_devices();
    app.place_startup_focus();
    // One USB device selects itself; the header owes it a port and a
    // connected icon.
    app.devices.set_devices(vec![DeviceInfo {
        port: "/dev/ttyACM0".into(),
        serial: None,
        vid_pid: "303a:1001".into(),
        description: "ESP32C3".into(),
    }]);
    app
}

#[test]
fn header_shows_the_backend_left_the_project_centered_the_device_right() {
    let mut app = header_fixture("wide");
    let frame = render(&mut app, 140, 32);
    let header = frame.lines().next().unwrap().trim_matches('"').to_string();

    assert!(
        header.contains("◆ Zephyr"),
        "the backend section names the kind under its icon:\n{header}"
    );
    assert!(
        header.contains("Project esp32c3_oled"),
        "the picked project must be the center:\n{header}"
    );
    assert!(
        header.trim_end().ends_with("● /dev/ttyACM0"),
        "the connected device rides the right edge:\n{header}"
    );

    // Centered on the whole bar: the project section's column equals half
    // the leftover width (`chars().count()` because `◆` is 3 bytes but
    // one column).
    let idx = header.find("Project").expect("the section to place");
    let column = header[..idx].chars().count();
    let section = "Project esp32c3_oled".chars().count();
    assert!(
        (140usize.saturating_sub(section)) / 2 == column,
        "the project section must sit centered, starts at {column}:\n{header}"
    );
}

#[test]
fn a_narrow_header_ellipsizes_the_project_and_never_the_device() {
    // At the declared minimum width, with a project name that cannot fit
    // beside the backend and device sections.
    const PROJECT: &str = "esp32c3_oled_with_a_very_long_name";
    let mut app = header_fixture_named("narrow", PROJECT);
    let frame = render(&mut app, 80, 32);
    let header = frame.lines().next().unwrap().trim_matches('"').to_string();

    assert!(
        header.contains("◆ Zephyr"),
        "the backend section keeps its shape:\n{header}"
    );
    assert!(
        header.contains("Project esp3"),
        "the project name ellipsizes, label kept:\n{header}"
    );
    assert!(
        !header.contains(PROJECT),
        "an 80-column line cannot hold the whole name:\n{header}"
    );
    assert!(
        header.trim_end().ends_with("● /dev/ttyACM0"),
        "the device status never truncates:\n{header}"
    );
}

#[test]
fn an_unscanned_header_shows_the_disconnect_icon_and_reason() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let frame = render(&mut app, 100, 32);
    let header = frame.lines().next().unwrap().trim_matches('"').to_string();

    assert!(
        header.contains("○ not scanned"),
        "before any scan the header says so, dimmed:\n{header}"
    );
    assert!(
        header.contains("▲ MicroPython"),
        "each backend kind carries its own icon:\n{header}"
    );
}

/// Under the Nerd set only MicroPython gets its own mark --- the Python
/// logo (`nf-custom-python`, the seti set): the FA5 brand proved missing
/// from partially patched fonts. Zephyr keeps its `◆` in every set.
#[test]
fn the_nerd_set_gives_micropython_the_python_logo_and_zephyr_keeps_its_mark() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.set_icon_set(chiptui::icons::IconSet::Nerd);
    let header = render(&mut app, 100, 32)
        .lines()
        .next()
        .unwrap()
        .trim_matches('"')
        .to_string();
    assert!(
        header.contains("\u{E73C} MicroPython"),
        "MicroPython's mark is the Python logo:\n{header}"
    );

    let mut app = app_with_backend(BackendKind::Zephyr);
    app.set_icon_set(chiptui::icons::IconSet::Nerd);
    let header = render(&mut app, 100, 32)
        .lines()
        .next()
        .unwrap()
        .trim_matches('"')
        .to_string();
    assert!(
        header.contains("◆ Zephyr"),
        "Zephyr's mark never changes with the set:\n{header}"
    );
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
    let frame = render(&mut app, 100, 32);

    assert!(
        frame.contains("Files:"),
        "missing the project-files pane:\n{frame}"
    );
    assert!(
        frame.contains("□ Zephyr path"),
        "the checklist must ask for the installation:\n{frame}"
    );
    assert!(
        frame.contains("Project path"),
        "the project checklist must ask for the project:\n{frame}"
    );
    assert!(
        frame.contains("Actions"),
        "the project panel must be titled as the actions pane:\n{frame}"
    );
    assert!(
        frame.contains("▶  Build"),
        "the lifecycle buttons must stay visible:\n{frame}"
    );
    assert!(
        frame.contains("─"),
        "the separator must divide checklist from buttons:\n{frame}"
    );
    assert!(
        !frame.contains("Device Files:"),
        "the dual-pane browser's device title must not render for a build backend:\n{frame}"
    );
    assert!(
        !frame.contains("Device Files:"),
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
    let frame = render(&mut app, 100, 32);

    assert!(
        frame.contains("Files:"),
        "missing the local file pane:\n{frame}"
    );
    assert!(
        !frame.contains("file browsing not implemented"),
        "MicroPython has a filesystem, no placeholder expected:\n{frame}"
    );
}

#[test]
fn row_three_shows_a_log_monitor_terminal_tab_strip() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let frame = render(&mut app, 100, 32);

    assert!(frame.contains("Log"), "missing the Log tab:\n{frame}");
    assert!(
        frame.contains("Monitor"),
        "missing the Monitor tab:\n{frame}"
    );
    assert!(
        frame.contains("Terminal"),
        "missing the Terminal tab:\n{frame}"
    );
}

#[test]
fn the_terminal_tab_renders_its_shell_session() {
    // The fake stands in for `$SHELL`, so the test never starts the
    // developer's real shell.
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.set_terminal_tool(chiptui::process::Command::new(format!(
        "{}/tests/fixtures/bin/slow",
        env!("CARGO_MANIFEST_DIR")
    )));
    app.focus = Focus::Logs;
    app.show_terminal_tab();
    let id = app.terminal_process.expect("the shell session started");

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Terminal"),
        "the strip still names the tab:\n{frame}"
    );
    // A live session with no output yet is a blank grid, not a message ---
    // the pane is a terminal now, and a terminal that has printed nothing
    // shows nothing but its cursor.
    assert!(
        !frame.contains("shell not running"),
        "a live session is not the spawn-failed state:\n{frame}"
    );
    assert!(
        frame.contains('\u{2588}'),
        "the shell's cursor block marks where typing lands:\n{frame}"
    );

    // The strip's status line names the shell it is running.
    assert!(
        frame.contains("slow"),
        "the status names the shell program:\n{frame}"
    );

    app.processes.cancel(id);
}

#[test]
fn switching_to_the_monitor_tab_changes_row_three() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.focus = Focus::Logs;
    let on_log = render(&mut app, 100, 32);

    app.log_tab = LogTab::Monitor;
    let on_monitor = render(&mut app, 100, 32);

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
    let frame = render(&mut app, 120, 32);

    assert!(
        frame.contains("Projects base"),
        "the project questions must render:\n{frame}"
    );
    assert!(frame.contains("MicroPython"), "missing backend:\n{frame}");
    assert!(
        !frame.contains("source:"),
        "the detection source no longer has a field:\n{frame}"
    );
}

#[test]
fn device_pane_prompts_to_open_flash_before_anything_has_been_queried() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let frame = render(&mut app, 100, 32);

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
    let frame = render(&mut app, 100, 32);

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

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Device Info"),
        "missing pane title:\n{frame}"
    );
    assert!(frame.contains("ESP32-S3"), "missing chip family:\n{frame}");
    assert!(frame.contains("revision 3"), "missing revision:\n{frame}");
    assert!(
        frame.contains("24:6f:28:12:34:56"),
        "missing MAC address:\n{frame}"
    );
}

/// A blank chip is an answer, not an unknown: the firmware row names the
/// erased flash instead of falling back to `undefined`.
#[test]
fn device_pane_names_erased_flash_as_no_firmware() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.details = DeviceDetails {
        family: Some(ChipFamily::Esp32),
        mac: Some("24:6f:28:12:34:56".to_string()),
        firmware: Some(FirmwareVerdict::Erased),
        ..DeviceDetails::default()
    };
    app.flash = Some(flash);

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("none (erased flash)"),
        "a blank chip must be named as blank:\n{frame}"
    );
    assert!(
        !frame.contains("undefined"),
        "erased is an answer, not an unknown:\n{frame}"
    );
}

/// The Firmware row shows the version's semver-ish prefix only --- a dev
/// build's git-describe suffix (`-N-gHASH`) is noise a fixed status line has
/// no room for and the user has no use for at a glance.
#[test]
fn device_pane_shortens_a_git_describe_version() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.details = DeviceDetails {
        family: Some(ChipFamily::Esp32C3),
        mac: Some("24:6f:28:12:34:56".to_string()),
        firmware: Some(FirmwareVerdict::Firmware(
            chiptui::firmware_id::FlashFirmware::Zephyr,
            Some("v4.4.0-11847-gc5dffcb7c9da".to_string()),
        )),
        ..DeviceDetails::default()
    };
    app.flash = Some(flash);

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Zephyr v4.4.0"),
        "the shortened version must still show:\n{frame}"
    );
    assert!(
        !frame.contains("11847"),
        "the git-describe suffix must not reach the Firmware row:\n{frame}"
    );
}

/// Row 2's top border carries its pane's title, so the line it sits on is
/// where row 1 ends: the same line in every backend, because the Project
/// and Device info panes are a fixed four content rows (`ui::panels`).
#[test]
fn row_one_is_the_same_fixed_height_in_both_backends() {
    let mut zephyr = app_with_backend(BackendKind::Zephyr);
    let mut micropython = app_with_backend(BackendKind::MicroPython);
    let z = render(&mut zephyr, 100, 32);
    let m = render(&mut micropython, 100, 32);

    let z_row2 = z.lines().position(|l| l.contains("Files"));
    let m_row2 = m.lines().position(|l| l.contains("Files"));
    // Header (1) + four content rows + the panes' borders (2) = row 2
    // starts on the eighth line.
    assert_eq!(
        z_row2,
        Some(7),
        "Zephyr's row 1 must be exactly four content rows:\n{z}"
    );
    assert_eq!(
        z_row2, m_row2,
        "row 1 must be equally tall in both backends:\n{z}\n{m}"
    );
}

#[test]
fn device_details_arriving_do_not_shift_the_dashboard() {
    let mut app = app_with_backend(BackendKind::MicroPython);
    let before = render(&mut app, 100, 32);
    let row2 = before
        .lines()
        .position(|l| l.contains("Files"))
        .expect("row 2 renders while the pane is a placeholder");

    // A full report fills all four content rows; the dashboard below must
    // not move a single line.
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.details = DeviceDetails {
        family: Some(ChipFamily::Esp32S3),
        revision: Some("3".to_string()),
        features: Some("Wi-Fi, BT 5 (LE), Dual Core + LP Core, 240MHz".to_string()),
        crystal_mhz: Some("40MHz".to_string()),
        mac: Some("24:6f:28:12:34:56".to_string()),
        ..DeviceDetails::default()
    };
    app.flash = Some(flash);

    let after = render(&mut app, 100, 32);
    assert!(
        after.contains("24:6f:28:12:34:56"),
        "the details must show:\n{after}"
    );
    assert_eq!(
        after.lines().position(|l| l.contains("Files")),
        Some(row2),
        "a full device report must not move row 2:\n{after}"
    );
}

#[test]
fn a_start_dir_below_the_project_root_rides_the_root_line() {
    // Detection climbed from the working directory to an ancestor with
    // evidence: the cwd no longer takes a row of its own (the pane is a
    // fixed four) --- it rides `root:`'s line as a muted suffix.
    let base = std::env::temp_dir().join(format!("chiptui-cwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let start = base.join("projects/app");
    std::fs::create_dir_all(&start).unwrap();
    for file in ["boot.py", "main.py", "config.py"] {
        std::fs::write(base.join(file), "").unwrap();
    }
    std::fs::create_dir_all(base.join("home")).unwrap();

    let mut app = App::new(&start);
    app.set_home_dir(base.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    let frame = render(&mut app, 100, 32);

    assert!(
        frame.contains("(cwd "),
        "the working directory must ride the root line:\n{frame}"
    );
    assert_eq!(
        frame.lines().position(|l| l.contains("Files")),
        Some(7),
        "the cwd suffix must not add a row to the pane:\n{frame}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// The declared minimum is a promise, not an aspiration: at exactly
/// `MIN_WIDTH`x`MIN_HEIGHT` every button the Zephyr pane offers must be
/// drawn (frame closed, not clipped a row short), the Device info pane must
/// keep all four of its rows, and row 3 must still have log to show. The
/// button stack is what sizes row 2, so anything added to it moves this
/// test --- which is the point: the constant and the layout cannot drift
/// apart in silence.
#[test]
fn the_declared_minimum_fits_the_whole_dashboard() {
    use chiptui::backend::micropython::esptool::{ChipFamily, DeviceDetails};

    let mut app = header_fixture("minimum");
    let mut flash = FlashPanel::new(std::env::temp_dir());
    // The widest realistic report: a chip line with both a revision and a
    // crystal is exactly what used to wrap at this width and push the
    // firmware row out of the pane.
    flash.details = DeviceDetails {
        family: Some(ChipFamily::Esp32S3),
        revision: Some("3".to_string()),
        // esptool's real ESP32-S3 line (`esptool/targets/esp32s3.py`'s
        // `get_chip_features`, flash and PSRAM both fitted): 97 characters
        // against the 27 this row has at the minimum width.
        features: Some(
            "Wi-Fi, BT 5 (LE), Dual Core + LP Core, 240MHz, Embedded Flash 8MB (XMC), \
             Embedded PSRAM 8MB (AP_3v3)"
                .to_string(),
        ),
        crystal_mhz: Some("40MHz".to_string()),
        mac: Some("24:6f:28:12:34:56".to_string()),
        firmware: Some(FirmwareVerdict::Firmware(
            chiptui::firmware_id::FlashFirmware::Zephyr,
            Some("v4.0.0".to_string()),
        )),
        ..DeviceDetails::default()
    };
    app.flash = Some(flash);

    let frame = render(&mut app, 80, 32);
    let lines: Vec<&str> = frame.lines().collect();

    // Six buttons, whichever half of the shared environment row is
    // showing: this fixture has no workspace, so the first is `Install
    // Zephyr` --- the row `Update Zephyr` becomes once one resolves. The
    // count is what the minimum was measured against, so the assertion is
    // on the count as much as on the labels.
    for label in [
        "Install Zephyr",
        "Menuconfig",
        "Clean",
        "Build",
        "Rebuild",
        "Flash",
    ] {
        assert!(
            frame.contains(label),
            "the `{label}` button is clipped at the declared minimum:\n{frame}"
        );
    }
    assert_eq!(
        app.build
            .as_ref()
            .unwrap()
            .actions(&app.manager.capabilities())
            .len(),
        6,
        "a seventh button would not fit the declared minimum"
    );

    // Not merely visible: the stack's own bottom rule has to follow the
    // last button, or the group is drawn a row short of its frame.
    let flash_row = lines
        .iter()
        .position(|line| line.contains("⇧  Flash"))
        .expect("the Flash button renders");
    assert!(
        lines[flash_row + 1].contains('╰'),
        "the button group is clipped one row short of its bottom rule:\n{frame}"
    );

    // Row 1 keeps all four rows: the chip line drops its suffixes rather
    // than wrapping, so the firmware answer stays in the pane.
    assert!(
        frame.contains("Firmware:") && frame.contains("Zephyr v4.0.0"),
        "the firmware row was pushed out of the Device info pane:\n{frame}"
    );

    // And the features row says something at this width. Truncating the raw
    // list got as far as `Wi-Fi, BT 5 (LE), Dual Core…`; compacting fits
    // every identifying fact whole, with the PSRAM size dropped off the
    // tail rather than a `…` standing in for it.
    let features_row = lines
        .iter()
        .find(|line| line.contains("Features:"))
        .expect("the features row renders");
    assert!(
        features_row.contains("WiFi, BLE5, 2x240MHz, 8MB"),
        "the features row was not compacted:\n{frame}"
    );
    assert!(
        !features_row.contains('…'),
        "the features row still truncates at the declared minimum:\n{frame}"
    );

    // And row 3 still has something to show.
    assert!(
        app.log_viewport >= 4,
        "the log pane is starved at the declared minimum: {} rows\n{frame}",
        app.log_viewport
    );
}

/// `ctrl+f` toggles row 3 (Log/Monitor/Terminal) to fill the whole
/// dashboard body, panes 1 and 2 undrawn, and hands focus to the only
/// pane still visible; a second press restores the normal layout.
#[test]
fn ctrl_f_toggles_row_3_fullscreen() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    app.focus = Focus::Project;

    let normal = render(&mut app, 80, 32);
    assert!(
        normal.contains("Environment") && normal.contains("Device Info"),
        "rows 1/2 show before the toggle:\n{normal}"
    );

    app.handle(ctrl('f'));
    assert!(app.row3_fullscreen, "ctrl+f arms the toggle");
    assert_eq!(
        app.focus,
        Focus::Logs,
        "fullscreen parks focus on the only pane still visible"
    );

    let full = render(&mut app, 80, 32);
    assert!(
        !full.contains("Environment") && !full.contains("Device Info"),
        "rows 1/2 stay undrawn while row 3 is fullscreen:\n{full}"
    );
    assert!(
        full.contains("Log"),
        "row 3's own tab strip still shows:\n{full}"
    );

    app.handle(ctrl('f'));
    assert!(
        !app.row3_fullscreen,
        "a second ctrl+f gives the layout back"
    );
    let restored = render(&mut app, 80, 32);
    assert!(
        restored.contains("Environment") && restored.contains("Device Info"),
        "rows 1/2 return once fullscreen is off:\n{restored}"
    );
}

/// While row 3 is fullscreen, `Tab`/`BackTab` are a no-op --- every other
/// pane is undrawn, so there is nowhere to step focus to.
#[test]
fn fullscreen_row_3_blocks_focus_stepping() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    app.handle(ctrl('f'));
    assert_eq!(app.focus, Focus::Logs);

    app.handle(key(KeyCode::Tab));
    assert_eq!(
        app.focus,
        Focus::Logs,
        "Tab must not move focus off the only visible pane"
    );

    app.handle(key(KeyCode::BackTab));
    assert_eq!(
        app.focus,
        Focus::Logs,
        "BackTab must not move focus off the only visible pane"
    );

    // The vertical chord is frozen the same way: the rows it would walk
    // are undrawn, so stepping anywhere would focus a pane that is not
    // there.
    app.handle(ctrl_key(KeyCode::Down));
    assert_eq!(
        app.focus,
        Focus::Logs,
        "ctrl+down must not step off the only visible pane"
    );
    app.handle(ctrl_key(KeyCode::Up));
    assert_eq!(
        app.focus,
        Focus::Logs,
        "ctrl+up must not step off the only visible pane"
    );
}

/// A `KeyEvent` with Ctrl held for codes beyond plain characters (the
/// arrow chords).
fn ctrl_key(code: KeyCode) -> chiptui::event::AppEvent {
    chiptui::event::AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        code,
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    ))
}

/// `ctrl+↓`/`ctrl+↑` walk the dashboard's *rows* following the screen's
/// geometry: each pane stays in its half (left/right), so the step lands
/// on the pane directly beneath/above, and row 3 --- full width --- is
/// entered from either side. The ends of the screen are the ends of the
/// walk.
#[test]
fn ctrl_arrows_step_focus_between_dashboard_rows() {
    let mut app = header_fixture("focus");
    // The Zephyr layout: row 1 = Environment | Device Info, row 2 =
    // Files | Actions, row 3 = Log • Monitor • Terminal.

    // The left half: Environment → the workspace pane beneath it.
    app.focus = Focus::Project;
    app.handle(ctrl_key(KeyCode::Down));
    assert_eq!(
        app.focus,
        Focus::Workspace,
        "ctrl+down from Environment stays in the left half"
    );
    // The right half: Device Info → the build panel beneath it (pane 4,
    // not pane 3).
    app.focus = Focus::DeviceInfo;
    app.handle(ctrl_key(KeyCode::Down));
    assert_eq!(
        app.focus,
        Focus::Build,
        "ctrl+down from Device Info stays in the right half"
    );
    // And upward the same way: the build panel rises to Device Info.
    app.handle(ctrl_key(KeyCode::Up));
    assert_eq!(
        app.focus,
        Focus::DeviceInfo,
        "ctrl+up from the build panel stays in the right half"
    );
    app.handle(ctrl_key(KeyCode::Up));
    assert_eq!(
        app.focus,
        Focus::DeviceInfo,
        "ctrl+up at the top is a no-op"
    );

    // Row 3 has no half: either column steps down into it, and leaving it
    // upward takes the left half, the reading order.
    app.focus = Focus::FilesDevice;
    app.handle(ctrl_key(KeyCode::Down));
    assert_eq!(app.focus, Focus::Logs, "ctrl+down reaches the log row");
    app.handle(ctrl_key(KeyCode::Down));
    assert_eq!(app.focus, Focus::Logs, "ctrl+down at the bottom is a no-op");
    app.handle(ctrl_key(KeyCode::Up));
    assert_eq!(
        app.focus,
        Focus::Workspace,
        "ctrl+up from row 3 takes the left half"
    );
    app.handle(ctrl_key(KeyCode::Up));
    assert_eq!(
        app.focus,
        Focus::Project,
        "ctrl+up reaches the Environment pane"
    );
}

/// `1`..`5` jump to the numbered panes --- numbers fixed per position
/// (1 Environment, 2 Device Info, 3/4 the working row, 5 row 3), the same
/// in every backend. A digit with no focusable pane behind it falls
/// through to the focused pane without moving focus.
#[test]
fn digits_jump_to_the_numbered_panes() {
    let mut app = header_fixture("focus");
    app.focus = Focus::Logs;

    app.handle(key(KeyCode::Char('1')));
    assert_eq!(app.focus, Focus::Project, "1 jumps to the Environment pane");
    app.handle(key(KeyCode::Char('2')));
    assert_eq!(app.focus, Focus::DeviceInfo, "2 jumps to Device Info");
    app.handle(key(KeyCode::Char('3')));
    assert_eq!(
        app.focus,
        Focus::Workspace,
        "3 jumps to the working row's left pane"
    );
    app.handle(key(KeyCode::Char('4')));
    assert_eq!(
        app.focus,
        Focus::Build,
        "4 jumps to the working row's right pane"
    );
    app.handle(key(KeyCode::Char('5')));
    assert_eq!(app.focus, Focus::Logs, "5 jumps to row 3");
    app.handle(key(KeyCode::Char('6')));
    assert_eq!(
        app.focus,
        Focus::Logs,
        "a digit with no pane behind it moves nothing"
    );
}

/// Every pane's title carries its fixed number at the leading edge --- the
/// digit `digits_jump_to_the_numbered_panes` presses. The off-tour panes
/// (Environment, Device Info) are numbered like the rest: the numbers
/// index the *panes*, not the `Tab` tour, so they are the same in every
/// backend.
#[test]
fn pane_titles_carry_their_shortcut_numbers() {
    let mut app = header_fixture("focus");
    let frame = render(&mut app, 100, 40);

    assert!(
        frame.contains("1 ☰ Environment"),
        "the Environment pane carries 1:\n{frame}"
    );
    assert!(
        frame.contains("2 ◆ Device Info"),
        "the Device Info pane carries 2:\n{frame}"
    );
    assert!(
        frame.contains("3 ▣ Files:"),
        "the workspace pane's title carries 3:\n{frame}"
    );
    assert!(
        frame.contains("4 ↯ Actions"),
        "the build pane's title carries 4:\n{frame}"
    );
    assert!(
        frame.contains("5 ▤ Log • ◉ Monitor • › Terminal"),
        "row 3's strip carries 5 at its leading edge:\n{frame}"
    );
}

/// The ctrl-arrow chord's horizontal half: a pane without a strip of its
/// own spends the chord on the pane beside it in the same row. Row 1's two
/// halves step between each other, and the Zephyr working row (no strips
/// anywhere) does the same.
#[test]
fn ctrl_arrows_step_focus_between_the_panes_of_a_row() {
    let mut app = header_fixture("focus");

    // Row 1: Environment ↔ Device Info.
    app.focus = Focus::Project;
    app.handle(ctrl_key(KeyCode::Right));
    assert_eq!(app.focus, Focus::DeviceInfo, "ctrl+→ crosses row 1");
    app.handle(ctrl_key(KeyCode::Left));
    assert_eq!(app.focus, Focus::Project, "ctrl+← crosses back");

    // Row 2: workspace ↔ build.
    app.focus = Focus::Workspace;
    app.handle(ctrl_key(KeyCode::Right));
    assert_eq!(app.focus, Focus::Build, "ctrl+→ crosses the working row");
    app.handle(ctrl_key(KeyCode::Left));
    assert_eq!(app.focus, Focus::Workspace, "ctrl+← crosses back");

    // Row 3 owns a strip, so its chord keeps switching tabs instead.
    app.focus = Focus::Logs;
    app.handle(ctrl_key(KeyCode::Right));
    assert_ne!(app.log_tab, LogTab::Log, "row 3's chord switches its strip");
}

/// Device Info is focusable (digit `2`, the chord from Environment) and
/// its MAC row arrives selected: `Enter` copies it through the same
/// clipboard request the row's click queues.
#[test]
fn enter_on_a_focused_device_info_copies_the_mac() {
    let mut app = header_fixture("focus");
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.details.mac = Some("24:6f:28:12:34:56".to_string());
    app.flash = Some(flash);

    app.focus = Focus::DeviceInfo;
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.take_clipboard_request(),
        Some("24:6f:28:12:34:56".to_string()),
        "Enter queues the MAC for the clipboard, like the row's click"
    );

    // Without a MAC read there is nothing to copy --- no request, no crash.
    app.flash = None;
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.take_clipboard_request(),
        None,
        "no MAC, no clipboard request"
    );
}

/// The footer advertises `ctrl+f` whenever row 3 holds focus --- the way
/// into fullscreen has to be discoverable from the Log tab, not just known
/// once already toggled.
#[test]
fn the_footer_advertises_ctrl_f_on_row_3() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    app.focus = Focus::Logs;

    let frame = render(&mut app, 140, 32);
    let footer = frame.lines().last().unwrap().trim_matches('"').to_string();

    assert!(
        footer.contains("ctrl+f") && footer.contains("fullscreen"),
        "row 3's footer is missing the fullscreen hint:\n{footer}"
    );
}

/// Output panes carry what the user is *reading*, so they never dim merely
/// because the cursor sits on another pane --- and the dashboard
/// deliberately parks focus on the build pane while a command streams, so
/// dimming on focus alone would dim exactly what is being watched. Behind a
/// dialog they dim like everything else: there the dashboard is context.
#[test]
fn output_panes_dim_behind_a_dialog_but_never_for_focus_alone() {
    use ratatui::style::Modifier;

    /// Cells of row 3 (the Log/Monitor pane) that carry `DIM`, and those
    /// that do not --- blanks ignored, they carry no text either way.
    fn row_three_dim(app: &mut App, width: u16, height: u16) -> (usize, usize) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| chiptui::ui::draw(frame, app))
            .expect("draw succeeds");
        let rendered = terminal.backend().to_string();
        let strip = rendered
            .lines()
            .position(|line| line.contains("Log"))
            .expect("row 3's tab strip") as u16;
        let buffer = terminal.backend().buffer().clone();
        let (mut dim, mut lit) = (0, 0);
        for y in (strip + 1)..height.saturating_sub(1) {
            for x in 1..width.saturating_sub(1) {
                let cell = &buffer[(x, y)];
                if cell.symbol().trim().is_empty() {
                    continue;
                }
                if cell.modifier.contains(Modifier::DIM) {
                    dim += 1;
                } else {
                    lit += 1;
                }
            }
        }
        (dim, lit)
    }

    let mut app = app_with_backend(BackendKind::Zephyr);
    app.logs
        .info("a log entry worth reading while a build runs");

    // The cursor is on another pane --- the state a running build puts the
    // user in. The feed stays lit.
    app.focus = Focus::Build;
    let (dim, lit) = row_three_dim(&mut app, 100, 40);
    assert!(
        lit > 0 && dim == 0,
        "the log feed dims while another pane holds the cursor: {dim} dim, {lit} lit"
    );

    // Focused: unchanged, still lit.
    app.focus = Focus::Logs;
    let (dim, lit) = row_three_dim(&mut app, 100, 40);
    assert!(
        lit > 0 && dim == 0,
        "the focused log feed must not dim either: {dim} dim, {lit} lit"
    );

    // Behind a dialog the whole dashboard is context, this pane included.
    app.overlay = Some(chiptui::app::OVERLAY_HELP);
    let (dim, _) = row_three_dim(&mut app, 100, 40);
    assert!(dim > 0, "the dashboard must dim behind a dialog");
}

/// `SPEC.md` §4.5 asks for *clear* confirmation of destructive actions.
/// Clear means three things a bare "Run <command>?" never said: which
/// action it is (the title), *what it happens to* (the target --- with two
/// boards plugged in, a dialog that names neither is answered blind), and
/// what is lost. The literal command stays quoted underneath (§15).
#[test]
fn destructive_confirmations_name_the_action_the_target_and_the_cost() {
    use chiptui::backend::BuildKind;
    use chiptui::backend::micropython::esptool::{ChipFamily, DeviceDetails};
    use chiptui::build::BuildAction;
    use chiptui::flash::FlashAction;

    // --- `west flash`: the board and the port it is plugged into ---
    let mut app = header_fixture("confirm-flash");
    app.build
        .as_mut()
        .expect("a build panel")
        .set_picked("esp32c3_devkitm");
    app.overlay = Some(Overlay::ConfirmBuild {
        action: BuildAction::Flash,
        confirm: false,
    });
    let frame = render(&mut app, 100, 34);
    assert!(
        frame.contains("Flash the board?"),
        "the title must name the action, not say `Confirm`:\n{frame}"
    );
    assert!(
        frame.contains("esp32c3_devkitm on /dev/ttyACM0"),
        "the dialog must name the board and port it writes to:\n{frame}"
    );
    assert!(
        frame.contains("Overwrites the firmware currently on it."),
        "the dialog must state what is lost:\n{frame}"
    );
    assert!(
        frame.contains("west flash"),
        "the literal command stays quoted:\n{frame}"
    );

    // --- `west build -t clean`: the project and the directory ---
    let mut app = header_fixture("confirm-clean");
    app.overlay = Some(Overlay::ConfirmBuild {
        action: BuildAction::Build(BuildKind::Clean),
        confirm: false,
    });
    let frame = render(&mut app, 100, 34);
    assert!(
        frame.contains("Clean the build?"),
        "the title must name the action:\n{frame}"
    );
    assert!(
        frame.contains("esp32c3_oled · build/"),
        "the dialog must name the directory it empties:\n{frame}"
    );
    assert!(
        frame.contains("west build -t clean"),
        "the literal command stays quoted:\n{frame}"
    );

    // --- esptool erase: the chip the background query already read ---
    let mut app = header_fixture("confirm-erase");
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.details = DeviceDetails {
        family: Some(ChipFamily::Esp32S3),
        ..DeviceDetails::default()
    };
    flash.request_confirmation(FlashAction::EraseFlash);
    app.flash = Some(flash);
    app.overlay = Some(Overlay::Confirm {
        message: "esptool --port /dev/ttyACM0 erase-flash".to_string(),
        confirm: false,
    });
    let frame = render(&mut app, 100, 34);
    assert!(
        frame.contains("Erase the flash?"),
        "the title must name the action:\n{frame}"
    );
    assert!(
        frame.contains("ESP32-S3 on /dev/ttyACM0"),
        "the dialog must name the chip it erases:\n{frame}"
    );
    assert!(
        frame.contains("firmware and filesystem alike"),
        "erasing everything must say so:\n{frame}"
    );

    // Nothing invented when the answers are missing: an unknown target
    // says it is unknown rather than reading as a confirmed board.
    let mut app = app_with_backend(BackendKind::MicroPython);
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.request_confirmation(FlashAction::EraseFlash);
    app.flash = Some(flash);
    app.overlay = Some(Overlay::Confirm {
        message: "esptool erase-flash".to_string(),
        confirm: false,
    });
    let frame = render(&mut app, 100, 34);
    assert!(
        frame.contains("no board selected"),
        "an unknown target must be named as unknown:\n{frame}"
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
        // The declared minimum and the two frames just under it, on each
        // axis: both must draw the size warning rather than a clipped
        // dashboard, and neither may panic on the way.
        (79, 32),
        (80, 31),
        (80, 32),
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

    app.overlay = Some(Overlay::Help {
        filter: String::new(),
        filtering: false,
        selected: 0,
    });
    let help = render(&mut app, 100, 32);
    assert!(
        help.contains("Navigation"),
        "navigation division missing:\n{help}"
    );
    assert!(
        help.contains("Commands"),
        "commands division missing:\n{help}"
    );
    assert!(
        help.contains("re-detect, reload, or rename (file list)"),
        "help body missing:\n{help}"
    );

    app.overlay = Some(Overlay::ProjectSetup { selected: 0 });
    let setup = render(&mut app, 100, 32);
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
fn theme_picker_leads_with_auto_over_the_fixed_themes() {
    // `Auto` is the one theme answer that depends on the session: it leads
    // the picker's rows with the two backends it maps to swatched side by
    // side, ahead of every fixed theme.
    let mut app = app_with_backend(BackendKind::Zephyr);
    app.overlay = Some(Overlay::ThemePicker { selected: 0 });
    let picker = render(&mut app, 100, 40);
    assert!(picker.contains("Auto"), "auto row missing:\n{picker}");
    assert!(
        picker.contains("(per backend)"),
        "auto row should say what it does:\n{picker}"
    );
    assert!(
        picker.contains("Catppuccin Mocha") && picker.contains("Everforest"),
        "fixed themes missing:\n{picker}"
    );
}

#[test]
fn selected_rows_carry_the_themes_selection_background() {
    use ratatui::style::Color;

    // A selected picker row must paint the theme's `selection` background
    // (with its `fg` on top), not reverse video --- the theme has to own
    // the highlight, or switching it leaves selections looking unchanged.
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.maybe_scan_devices();
    app.overlay = Some(Overlay::ThemePicker { selected: 0 });

    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    let palette = app.theme_palette();
    terminal
        .draw(|frame| chiptui::ui::draw(frame, &mut app))
        .expect("draw succeeds");
    let buffer = terminal.backend().buffer().clone();

    let selection = palette.selection;
    let mut painted = 0;
    let mut readable = false;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if cell.bg == selection {
                painted += 1;
                readable |= cell.fg == palette.fg;
            }
        }
    }
    assert!(
        painted > 0,
        "the selected row must be filled with palette.selection"
    );
    assert!(
        readable,
        "the selected row's text must be palette.fg on palette.selection"
    );
    assert_ne!(selection, Color::Reset);
}

/// The focused pane carries a subtle background tint derived from the
/// theme: the accent blended 1/16 toward the theme's own background, over
/// the pane's whole rect, while every other pane stays transparent. The
/// tint follows focus when it moves, and it stays a whisper --- each
/// channel of it sits within a few steps of the theme's background
/// (1/64 toward the accent), far subtler than the home rows' 3/16
/// resting tint.
#[test]
fn the_focused_pane_carries_a_subtle_theme_tint() {
    use ratatui::style::Color;

    // The expected tint, recomputed here from the palette the app draws
    // with: `blend(accent, bg, 1, 64)`, one channel at a time.
    let tint_of = |palette: ratatui_themes::ThemePalette| -> Color {
        let Color::Rgb(r1, g1, b1) = palette.accent else {
            panic!("the theme's accent is an RGB value");
        };
        let Color::Rgb(r2, g2, b2) = palette.bg else {
            panic!("the theme's background is an RGB value");
        };
        let channel =
            |toward: u8, from: u8| (from as i32 + (toward as i32 - from as i32) / 64) as u8;
        Color::Rgb(channel(r1, r2), channel(g1, g2), channel(b1, b2))
    };

    let mut app = header_fixture("tint");
    let palette = app.theme_palette();
    let tint = tint_of(palette);
    // The wash must still exist: on every shipped theme the accent differs
    // from the background enough that a 64th moves at least one channel.
    assert_ne!(
        tint, palette.bg,
        "a 64th of the accent still shifts the background"
    );

    // Nearly imperceptible by construction: each channel of the tint sits
    // a couple of steps from the theme's background at most (1/64 of the
    // way toward the accent).
    if let (Color::Rgb(tr, tg, tb), Color::Rgb(br, bg_, bb)) = (tint, palette.bg) {
        for (t, b) in [(tr, br), (tg, bg_), (tb, bb)] {
            assert!(
                (i16::from(t) - i16::from(b)).unsigned_abs() <= 8,
                "the tint strays too far from the theme's background"
            );
        }
    }

    let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("test terminal");
    let draw = |terminal: &mut Terminal<TestBackend>, app: &mut App| {
        terminal
            .draw(|frame| chiptui::ui::draw(frame, app))
            .expect("draw succeeds");
        terminal.backend().buffer().clone()
    };

    // Focus on the workspace pane: its rect is tinted (the header's own
    // accent cells and the home-style row tints are painted by their own
    // rules; the pane tint is the only whole-rect wash on the dashboard).
    app.focus = Focus::Workspace;
    let buffer = draw(&mut terminal, &mut app);
    let mut tinted_first: Vec<(u16, u16)> = Vec::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            if buffer[(x, y)].bg == tint {
                tinted_first.push((x, y));
            }
        }
    }
    assert!(
        tinted_first.len() > 100,
        "the pane's whole interior is tinted: {} cells",
        tinted_first.len()
    );
    // And the tinted rect is one contiguous pane: a single row through its
    // middle carries a long unbroken run of it.
    let longest_run = (0..buffer.area.height)
        .map(|y| {
            let mut run = 0;
            let mut best = 0;
            for x in 0..buffer.area.width {
                if buffer[(x, y)].bg == tint {
                    run += 1;
                    best = best.max(run);
                } else {
                    run = 0;
                }
            }
            best
        })
        .max()
        .unwrap();
    assert!(
        longest_run > 40,
        "the tint covers a pane-wide rect, not scattered cells: {longest_run}"
    );

    // The borders are *not* tinted: the wash is the pane's interior only,
    // so the frame stays the terminal's own background. The workspace pane
    // spans the frame's left edge (column 0 is its left border), and its
    // top/bottom border rows are the frame's rules --- none of those may
    // carry the tint. (Rows inside may legitimately break the run where
    // content paints its own background, e.g. the selected checklist row's
    // `palette.selection` --- that is the wash sitting *under* the content,
    // the documented order.)
    for (x, y) in &tinted_first {
        assert!(
            *x != 0 && *x != buffer.area.width - 1,
            "the pane's left/right border columns carry no tint ({x},{y})"
        );
    }
    let row_text = |buffer: &ratatui::buffer::Buffer, y: u16| -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect()
    };
    let mut border_rows_checked = 0;
    for y in 0..buffer.area.height {
        let text = row_text(&buffer, y);
        let border_row = (text.contains("Files:") && text.starts_with('\u{256d}'))
            || text.starts_with('\u{2570}');
        if border_row {
            assert!(
                (0..buffer.area.width).all(|x| buffer[(x, y)].bg != tint),
                "a border row of the focused pane carries the tint: {text}"
            );
            border_rows_checked += 1;
        }
    }
    assert!(
        border_rows_checked >= 2,
        "the pane's top and bottom border rows were both found and checked"
    );

    // The tint follows focus: moving to the build pane returns every
    // previously tinted cell to transparency (the panes are disjoint
    // rects) and paints the sibling instead.
    app.focus = Focus::Build;
    let buffer = draw(&mut terminal, &mut app);
    let mut tinted_second = 0;
    for (x, y) in &tinted_first {
        assert_eq!(
            buffer[(*x, *y)].bg,
            Color::Reset,
            "the workspace pane lost the tint when focus moved"
        );
    }
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            if buffer[(x, y)].bg == tint {
                tinted_second += 1;
            }
        }
    }
    assert!(
        tinted_second > 100,
        "the build pane picked the tint up: {tinted_second} cells"
    );
}

#[test]
fn help_fits_one_line_per_binding_and_scrolls_under_the_cursor() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    let last = help::bindings(app.view, HelpSection::Commands).len() - 1;
    app.overlay = Some(Overlay::Help {
        filter: String::new(),
        filtering: false,
        selected: last,
    });

    // Wide enough for the whole table: every binding stays on one line ---
    // a description leaking onto a second line means the text outgrew the
    // single-window budget.
    let wide = render(&mut app, 100, 32);
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

    // Too short for the whole table (thirty-five rows against the
    // minimum's thirty-two): the cursor keeps the last command on screen
    // by scrolling the list.
    let short = render(&mut app, 100, 32);
    assert!(
        short.contains("q / ctrl+c"),
        "the last binding was cut off vertically:\n{short}"
    );
}

#[test]
fn the_help_window_narrows_under_the_filter() {
    let mut app = app_with_backend(BackendKind::Zephyr);
    app.overlay = Some(Overlay::Help {
        filter: "sync".to_string(),
        filtering: true,
        selected: 0,
    });

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("filter sync"),
        "the filter line is missing:\n{frame}"
    );
    assert!(
        frame.contains("shift+s"),
        "the row whose description mentions the filter is missing:\n{frame}"
    );
    assert!(
        !frame.contains("override the detected backend"),
        "rows the filter excludes are still drawn:\n{frame}"
    );
    // A section that matched nothing disappears with its title.
    assert!(
        !frame.contains("Navigation"),
        "an empty section keeps its title:\n{frame}"
    );
}

#[test]
fn the_renderer_publishes_the_log_viewport_height() {
    // The log pane's height drives page-scrolling, so it must reflect the frame.
    let mut app = app_with_backend(BackendKind::Zephyr);

    render(&mut app, 100, 44);
    let tall = app.log_viewport;
    render(&mut app, 100, 32);
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
    let with_files_focus = render(&mut app, 100, 32);

    app.focus = Focus::Logs;
    let with_log_focus = render(&mut app, 100, 32);

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

    let frame = render(&mut app, 100, 32);
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

    let frame = render(&mut app, 100, 32);
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
    let scrolled = render(&mut app, 100, 32);
    assert_ne!(frame, scrolled, "scrolling must move the thumb");
}

#[test]
fn the_monitor_tab_shows_the_same_scrollbar_once_output_overflows_it() {
    // The Monitor tab always tails its live output, so its scrollbar is an
    // indicator: shown when the wrapped console outgrows the pane, thumb
    // pinned to the bottom.
    let mut app = app_with_backend(BackendKind::MicroPython);
    app.log_tab = LogTab::Monitor;

    let short = render(&mut app, 100, 32);
    assert!(
        !short.contains('┃'),
        "no scrollbar before the session has output:\n{short}"
    );

    for i in 0..60 {
        app.device_monitor_output
            .push(format!("monitor line {i:02}"));
    }
    let frame = render(&mut app, 100, 32);
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
    let cleared = render(&mut app, 100, 32);
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

    let tail = render(&mut app, 100, 32);
    assert!(
        !tail.contains("monitor line 00"),
        "the tail view shows the newest output:\n{tail}"
    );

    app.handle(key(KeyCode::Home));
    let top = render(&mut app, 100, 32);
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
    let held = render(&mut app, 100, 32);
    assert!(
        held.contains("monitor line 00"),
        "live output must not shift a scrolled monitor view:\n{held}"
    );

    app.handle(key(KeyCode::End));
    let bottom = render(&mut app, 100, 32);
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
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("▤ Log • ◉ Monitor"),
        "the tabs must share the border row with the dot divider:\n{frame}"
    );

    // Switching tabs highlights the other title on the same border --- the
    // chord is the tab key now (the plain arrows switch nothing).
    app.focus = Focus::Logs;
    app.handle(ctrl_key(KeyCode::Right));
    assert_eq!(app.log_tab, LogTab::Monitor);
}

/// The tab strip draws over the pane's top border row, and the `Tabs`
/// widget's base style is applied to *every* cell of that row --- the border
/// rules included. That base style must be the border's own color
/// (`border_style`), not the inactive label's muted: with the muted one the
/// focused frame read accent everywhere except the top edge the strip
/// repainted.
#[test]
fn the_tab_strip_keeps_the_focused_pane_top_border_accent() {
    use ratatui::style::Color;

    /// The fg colors the top-border rule cells (`─`) carry on the row that
    /// holds `needle` --- the border row a tab strip draws over.
    fn top_rules(app: &mut App, needle: &str) -> Vec<Color> {
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
        terminal
            .draw(|frame| chiptui::ui::draw(frame, app))
            .expect("draw succeeds");
        let rendered = terminal.backend().to_string();
        let y = rendered
            .lines()
            .position(|line| line.contains(needle))
            .expect("the tab strip's row") as u16;
        let buffer = terminal.backend().buffer().clone();
        (1..99)
            .map(|x| buffer[(x, y)].clone())
            .filter(|cell| cell.symbol() == "\u{2500}")
            .map(|cell| cell.fg)
            .collect()
    }

    let mut app = app_with_backend(BackendKind::Zephyr);

    // Focus elsewhere: the whole frame, the top border carried by the strip
    // included, reads muted.
    app.focus = Focus::Build;
    let rules = top_rules(&mut app, "▤ Log • ◉ Monitor");
    assert!(!rules.is_empty(), "the strip's row must carry border rules");
    assert!(
        rules.iter().all(|fg| *fg == app.theme_palette().muted),
        "an unfocused top border must read muted: {rules:?}"
    );

    // The pane holds focus: the strip repaints the border row, so its rules
    // must keep the frame's accent, not take the labels' muted.
    app.focus = Focus::Logs;
    let rules = top_rules(&mut app, "▤ Log • ◉ Monitor");
    assert!(
        rules.iter().all(|fg| *fg == app.theme_palette().accent),
        "the focused top border must stay accent under the tab strip: {rules:?}"
    );
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

    let frame = render(&mut app, 100, 32);
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
        cancelled: false,
        duration: std::time::Duration::from_secs(3),
        at: time::OffsetDateTime::now_utc(),
    };
    app.build.as_mut().unwrap().last = Some(report(true));
    let ok = render(&mut app, 100, 32);
    assert!(
        ok.contains("✓ Build ("),
        "a finished command must be checked green with its row count:\n{ok}"
    );

    app.build.as_mut().unwrap().last = Some(report(false));
    let failed = render(&mut app, 100, 32);
    assert!(
        failed.contains("✗ Build ("),
        "a failed command must be crossed red:\n{failed}"
    );

    // A stopped command carries its own mark on the strip, matching the
    // pane's footer: not the error cross (stopping is what the user asked
    // for) and not the success check either (nothing finished).
    app.build.as_mut().unwrap().last = Some(chiptui::build::BuildReport {
        what: "Build",
        ok: false,
        cancelled: true,
        duration: std::time::Duration::from_secs(3),
        at: time::OffsetDateTime::now_utc(),
    });
    let stopped = render(&mut app, 100, 32);
    assert!(
        stopped.contains("◼ Build ("),
        "a stopped command must carry the stopped mark:\n{stopped}"
    );
    assert!(
        !stopped.contains("✗ Build (") && !stopped.contains("✓ Build ("),
        "a stopped command is neither an error nor a success:\n{stopped}"
    );

    // Before any command ran, the title stands alone with the row count.
    app.build.as_mut().unwrap().last = None;
    let bare = render(&mut app, 100, 32);
    assert!(
        bare.contains("Build ("),
        "the source title still names the feed:\n{bare}"
    );
    assert!(
        !bare.contains("✓ Build") && !bare.contains("✗ Build"),
        "no verdict without a command:\n{bare}"
    );
}
