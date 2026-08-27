//! The esptool flash/erase view end to end, against the fake `esptool`.
//!
//! Mirrors `tests/files_view.rs`: a real process producing real bytes, driven
//! through `App` exactly as `main.rs` does (`app.processes.drain()` fed back
//! through `AppEvent::Process`), so the confirmation gate and the post-erase
//! flash offer are exercised the same way a user would trigger them.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chiptui::app::{App, Focus, LogTab, MonitorSource, Overlay, View};
use chiptui::backend::BackendKind;
use chiptui::backend::micropython::esptool::ChipFamily;
use chiptui::browser::Browser;
use chiptui::device::{DeviceInfo, ScriptState};
use chiptui::event::AppEvent;
use chiptui::flash::{FlashAction, FlashPanel, FlashScreen, RunState};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{fake_curl, fake_mpremote, key, render};

fn fake_esptool() -> String {
    format!("{}/tests/fixtures/bin/esptool", env!("CARGO_MANIFEST_DIR"))
}

fn fake_esptool_progress_slow() -> String {
    format!(
        "{}/tests/fixtures/bin/esptool-progress-slow",
        env!("CARGO_MANIFEST_DIR")
    )
}

struct Project {
    root: PathBuf,
}

impl Project {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("chiptui-flash-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write_firmware(&self, name: &str) {
        let dir = self.root.join("firmware");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), b"x").unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A home directory that does not exist, unique per call --- the same
/// hermeticity `tests/ui_render.rs`'s `scratch_home` buys: `App::new` reads
/// `[ui] theme`/`[ui] icons` out of `$HOME`'s config, so without this the
/// frames assert against whatever the developer happens to have configured
/// (an `icons = "nerd"` in the real config turned every glyph assertion
/// here into a coin flip).
fn hermetic_app(root: impl Into<PathBuf>) -> App {
    common::hermetic_app(root, "chiptui-flash-home")
}

/// An app sitting in the flash view, pointed at the fake `esptool`.
fn app_with_flash(project: &Project) -> App {
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.open_flash();
    app.flash.as_mut().unwrap().set_tool_path(fake_esptool());
    app
}

/// Drives the app until the flash panel is no longer busy.
fn settle(app: &mut App) {
    common::settle_while(
        app,
        |app| app.flash.as_ref().is_some_and(|flash| flash.is_busy()),
        "esptool command",
    );
}

/// Drives the app until `done` holds or time runs out --- for asserting on
/// an in-flight state (`settle` above only ever waits for the *end*).
fn pump_until(app: &mut App, mut done: impl FnMut(&App) -> bool, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        if done(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    done(app)
}

#[test]
fn the_flash_view_is_gated_on_flash_capabilities() {
    // AGENTS.md §3: the gate is the capability, not the backend name. No
    // backend is detected in a bare temp directory, so neither `Flash` nor
    // `EraseFlash` is available.
    let mut app = hermetic_app(std::env::temp_dir());
    app.bootstrap();
    app.open_flash();
    assert_eq!(app.view, View::Dashboard, "no backend exposes no flash op");
    assert!(app.flash.is_none());

    app.manager.set_override(Some(BackendKind::MicroPython));
    app.open_flash();
    assert_eq!(app.view, View::Flash);
    assert!(app.flash.is_some());
}

#[test]
fn read_only_actions_run_without_confirmation() {
    let project = Project::new("chip-info");
    let mut app = app_with_flash(&project);

    // Cursor starts on the first action, "chip information".
    app.handle(key(KeyCode::Enter));
    settle(&mut app);

    let flash = app.flash.as_ref().unwrap();
    assert!(
        matches!(flash.state, RunState::Succeeded),
        "state was {:?}",
        flash.state
    );
    assert_eq!(
        app.view,
        View::Dashboard,
        "running an action closes the flash dialog"
    );
    assert_eq!(app.focus, Focus::Logs);
    assert_eq!(app.log_tab, LogTab::Monitor);
    assert_eq!(app.monitor_source, MonitorSource::Flash);
    assert_eq!(app.overlay, None, "nothing destructive, nothing to confirm");
}

#[test]
fn erase_flash_requires_confirmation_showing_the_literal_command() {
    let project = Project::new("erase-confirm");
    let mut app = app_with_flash(&project);

    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down)); // erase flash
    app.handle(key(KeyCode::Enter));

    match &app.overlay {
        Some(Overlay::Confirm { message, .. }) => {
            assert!(
                message.contains("erase-flash"),
                "confirmation must show the real command, not a paraphrase: {message}"
            );
        }
        other => panic!("expected a confirmation overlay, got {other:?}"),
    }
    // Nothing ran yet.
    assert!(!app.flash.as_ref().unwrap().is_busy());
}

#[test]
fn cancelling_the_confirmation_runs_nothing() {
    let project = Project::new("erase-cancel");
    let mut app = app_with_flash(&project);

    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::Confirm { .. })));

    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
    let flash = app.flash.as_ref().unwrap();
    assert!(!flash.is_busy());
    assert_eq!(flash.state, RunState::Idle);
}

#[test]
fn erase_then_offer_never_flashes_on_its_own() {
    let project = Project::new("erase-offer");
    project.write_firmware("app.bin");
    let mut app = app_with_flash(&project);

    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down)); // erase flash
    app.handle(key(KeyCode::Enter)); // opens the confirm overlay
    app.handle(key(KeyCode::Char('y'))); // accepts it
    settle(&mut app);

    let flash = app.flash.as_ref().unwrap();
    assert!(
        matches!(flash.state, RunState::Succeeded),
        "the erase itself ran"
    );
    assert_eq!(
        flash.screen,
        FlashScreen::Options,
        "the single firmware file found was offered automatically"
    );
    assert_eq!(flash.selected_action(), FlashAction::WriteFlash);
    assert_eq!(flash.selected_firmware, Some(0));
    assert_eq!(
        app.overlay, None,
        "landing on the options screen is not itself a write"
    );
}

#[test]
fn several_firmware_candidates_open_a_picker() {
    let project = Project::new("multi-firmware");
    project.write_firmware("a.bin");
    project.write_firmware("b.bin");
    let mut app = app_with_flash(&project);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down)); // chip info, flash info, erase flash, -> write flash
    }
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, Some(Overlay::FirmwarePicker { selected: 0 }));

    app.handle(key(KeyCode::Enter)); // choose the first
    assert_eq!(app.overlay, None);
    let flash = app.flash.as_ref().unwrap();
    assert_eq!(flash.screen, FlashScreen::Options);
    assert_eq!(flash.selected_firmware, Some(0));
}

#[test]
fn write_flash_end_to_end_succeeds_and_detects_the_chip() {
    let project = Project::new("write-e2e");
    project.write_firmware("app.bin");
    let mut app = app_with_flash(&project);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter)); // discovers the lone firmware, jumps to Options
    assert_eq!(app.flash.as_ref().unwrap().screen, FlashScreen::Options);

    // Type an offset directly rather than picking a chip, so chip family
    // detection below observes the esptool banner rather than a manual pick.
    app.handle(key(KeyCode::Tab)); // Chip -> Offset
    for c in "0x1000".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter)); // request confirmation
    match &app.overlay {
        Some(Overlay::Confirm { message, .. }) => {
            assert!(message.contains("write-flash"));
            assert!(message.contains("app.bin"));
            assert!(message.contains("0x1000"));
        }
        other => panic!("expected a confirmation overlay, got {other:?}"),
    }

    app.handle(key(KeyCode::Char('y'))); // confirm
    settle(&mut app);

    let flash = app.flash.as_ref().unwrap();
    assert!(
        matches!(flash.state, RunState::Succeeded),
        "state was {:?}",
        flash.state
    );
    assert_eq!(
        flash.chip.family(),
        Some(ChipFamily::Esp32),
        "the esptool banner was picked up"
    );
}

/// Item 05 of the 2026-08-20 UX audit: the state line must show esptool's
/// own write percentage while it streams in, not just a stopwatch, and go
/// back to the counted duration once the command finishes. Driven through
/// the device pane's **Project actions** tab (`x`), the path that actually
/// renders `draw_action_state` --- `app_with_flash`'s standalone dialog
/// switches away to the Monitor tab the instant a command starts
/// (`App::show_flash_in_monitor`), so it never shows this line at all.
///
/// Firmware and offset are set directly on the panel rather than walked
/// through the Options screen: reaching Options from this tab by pressing
/// `Enter` on Write/flash does not open it as a dialog (only
/// `offer_flash_after_erase` does that), so a firmware file discovered this
/// way sits unconfirmable on screen --- a preexisting gap, not something
/// this test is about. Pre-selecting both means `Enter` here takes the
/// already-armed, destructive branch straight to the confirm overlay
/// instead, same as erase.
#[test]
fn a_running_write_flash_reports_esptools_percentage() {
    let project = Project::new("write-progress");
    project.write_firmware("app.bin");
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.handle(key(KeyCode::Char('x')));
    assert!(
        app.device_actions_tab_active(),
        "the actions tab must be showing"
    );
    let flash = app.flash.as_mut().unwrap();
    flash.set_tool_path(fake_esptool_progress_slow());
    flash.discover_firmware();
    assert!(flash.select_firmware(0));
    flash.set_offset("0x1000".to_string());

    for _ in 0..5 {
        app.handle(key(KeyCode::Down)); // ... -> write / flash firmware
    }
    app.handle(key(KeyCode::Enter)); // firmware and offset already set: straight to confirm
    match &app.overlay {
        Some(Overlay::Confirm { message, .. }) => assert!(message.contains("write-flash")),
        other => panic!("expected a confirmation overlay, got {other:?}"),
    }
    app.handle(key(KeyCode::Char('y'))); // confirm

    assert!(app.flash.as_ref().unwrap().is_busy());
    assert!(
        app.device_actions_tab_active(),
        "still on the tab once running"
    );
    let caught_progress = pump_until(
        &mut app,
        |app| app.flash.as_ref().unwrap().progress().is_some(),
        10,
    );
    assert!(caught_progress, "no esptool percentage was ever parsed");
    assert_eq!(
        app.flash.as_ref().unwrap().progress(),
        Some(chiptui::progress::Progress::Percent(10))
    );
    // Wide enough that the state line's half of the (already half-width)
    // actions pane fits the label and the percentage both.
    let frame = render(&mut app, 200, 40);
    assert!(
        frame.contains("Write / flash firmware · 10%"),
        "state line missing the percentage:\n{frame}"
    );

    settle(&mut app);
    let flash = app.flash.as_ref().unwrap();
    assert!(
        matches!(flash.state, RunState::Succeeded),
        "state was {:?}",
        flash.state
    );
    assert!(
        flash.progress().is_none(),
        "progress must clear once the command is no longer running"
    );
}

#[test]
fn write_flash_without_a_firmware_file_or_chip_stays_on_the_menu() {
    // No chip is known either, so there is nothing to search for: the
    // action dead-ends with the reason instead of opening anything.
    let project = Project::new("no-firmware");
    let mut app = app_with_flash(&project);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));

    let flash = app.flash.as_ref().unwrap();
    assert_eq!(flash.screen, FlashScreen::Menu, "nothing to configure yet");
    assert_eq!(app.overlay, None);
    assert!(
        app.logs
            .visible(10)
            .any(|entry| entry.message.contains("no chip known yet")),
        "the reason must be logged"
    );
}

#[test]
fn write_flash_with_an_empty_folder_opens_the_online_search_window() {
    // The chip is already known, firmware/ is empty: selecting
    // write / flash must open the online search (the window states its
    // source and that a local file would outrank it) rather than
    // dead-ending on a warning.
    let project = Project::new("no-firmware-search");
    let mut app = app_with_online_search(&project);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));

    let flash = app.flash.as_ref().unwrap();
    assert_eq!(flash.screen, FlashScreen::OnlineBoards);
    assert!(flash.is_busy(), "the board search is already running");
    assert_eq!(
        flash.online_source.as_deref(),
        Some("https://micropython.org/download/?mcu=esp32")
    );
    assert_eq!(app.view, View::Flash);

    settle(&mut app);
    assert_eq!(app.flash.as_ref().unwrap().online_boards.len(), 2);
}

#[test]
fn the_online_search_window_names_its_source_and_the_local_folder_priority() {
    let project = Project::new("no-firmware-render");
    let mut app = app_with_online_search(&project);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    let frame = render(&mut app, 110, 32);

    assert!(
        frame.contains("micropython.org/download/?mcu=esp32"),
        "the source being queried must be visible, not implied:\n{frame}"
    );
    assert!(
        frame.contains("no .bin/.elf in firmware/ yet"),
        "the window must say the local folder is empty:\n{frame}"
    );
    assert!(
        frame.contains("picked first"),
        "a firmware added to the folder must be said to come first:\n{frame}"
    );

    settle(&mut app);
    let frame = render(&mut app, 110, 32);
    assert!(
        frame.contains("2 boards for this chip"),
        "results carry a status line:\n{frame}"
    );
    assert!(
        frame.contains("Vendor") && frame.contains("Firmware") && frame.contains("Board"),
        "the boards window is a table with named columns:\n{frame}"
    );
    let vendor = frame.find("Vendor").expect("checked above");
    assert!(
        frame[vendor..].contains("Firmware"),
        "the Vendor column must lead the Firmware column:\n{frame}"
    );
    assert!(
        frame.contains("Espressif") && frame.contains("Acme"),
        "every vendor's boards arrive, the vendor is a column to choose from:\n{frame}"
    );
}

#[test]
fn write_flash_without_a_chip_or_offset_stays_on_options_instead_of_confirming() {
    // Regression: confirming here used to build `esptool write-flash "" file`
    // --- an empty positional offset --- because nothing had populated the
    // offset field yet. It must block instead of opening the confirm overlay.
    let project = Project::new("no-offset");
    project.write_firmware("app.bin");
    let mut app = app_with_flash(&project);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter)); // discovers the lone firmware, jumps to Options
    assert_eq!(app.flash.as_ref().unwrap().screen, FlashScreen::Options);

    app.handle(key(KeyCode::Enter)); // no chip picked yet: must not confirm
    assert_eq!(
        app.overlay, None,
        "a blank offset must never reach the confirm overlay"
    );
    assert_eq!(app.flash.as_ref().unwrap().screen, FlashScreen::Options);
    assert!(
        app.logs
            .visible(10)
            .any(|entry| entry.message.contains("set a flash offset")),
        "the reason must be logged"
    );

    // Picking a chip fills a default offset, which unblocks confirmation.
    app.handle(key(KeyCode::Right));
    app.handle(key(KeyCode::Enter));
    match &app.overlay {
        Some(Overlay::Confirm { message, .. }) => assert!(
            !message.contains("write-flash  ") && !message.contains("write-flash \""),
            "offset must not be blank in the confirmed command: {message}"
        ),
        other => panic!("expected a confirmation overlay once unblocked, got {other:?}"),
    }
}

#[test]
fn the_flash_menu_flags_destructive_actions() {
    let project = Project::new("render-menu");
    let mut app = app_with_flash(&project);
    let frame = render(&mut app, 110, 32);

    assert!(frame.contains("Erase flash"), "{frame}");
    assert!(frame.contains("confirm"), "{frame}");
}

#[test]
fn the_flash_menu_shows_an_icon_per_action() {
    let project = Project::new("render-menu-icons");
    let mut app = app_with_flash(&project);
    let frame = render(&mut app, 110, 32);

    assert!(frame.contains("⌫ Erase flash"), "{frame}");
    assert!(frame.contains("⇪ Write / flash firmware"), "{frame}");
}

#[test]
fn the_flash_dialog_shrinks_to_fit_its_content_instead_of_filling_the_screen() {
    // A real dialog, not a near-fullscreen replacement: on a large terminal
    // the menu's own bordered box must be far narrower than the body, with
    // dashboard panes still visible on either side of it.
    let project = Project::new("render-dialog-sizing");
    let mut app = app_with_flash(&project);
    let frame = render(&mut app, 160, 40);

    let border_line = frame
        .lines()
        .find(|line| line.contains("╭ Flash"))
        .unwrap_or_else(|| panic!("no Flash dialog border in frame:\n{frame}"));
    let chars: Vec<char> = border_line.chars().collect();
    let start = chars.iter().position(|&c| c == '╭').unwrap();
    let end = chars.iter().rposition(|&c| c == '╮').unwrap();
    let box_width = end - start + 1;

    assert!(
        box_width < 80,
        "the flash menu should be a small content-fit dialog, not half the screen: \
         {box_width} cols\n{border_line}"
    );
    assert!(
        start > 0,
        "the dialog should be inset from the left edge, with dashboard content beside it:\n{border_line}"
    );
}

#[test]
fn write_flash_output_shows_a_read_only_recap_above_the_console() {
    let project = Project::new("render-output-recap");
    project.write_firmware("app.bin");
    let mut app = app_with_flash(&project);

    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter)); // discovers the lone firmware, jumps to Options
    app.handle(key(KeyCode::Tab)); // Chip -> Offset
    for c in "0x1000".chars() {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter)); // request confirmation
    app.handle(key(KeyCode::Char('y'))); // confirm
    settle(&mut app);

    // Row 3 (the Monitor tab) is a fixed fraction of the dashboard rather
    // than a near-fullscreen dialog, so this needs a taller terminal than a
    // dialog-only render did to leave room for the console below the recap.
    let frame = render(&mut app, 110, 45);
    assert!(frame.contains("0x1000"), "missing offset recap:\n{frame}");
    assert!(
        frame.contains("app.bin"),
        "missing firmware recap:\n{frame}"
    );
    assert!(frame.contains("console"), "missing console label:\n{frame}");
    assert!(frame.contains("Wrote"), "missing streamed output:\n{frame}");
}

#[test]
fn a_read_only_action_s_output_has_no_recap_section() {
    // `ChipInfo` never goes through the Options screen, so there is nothing
    // to recap --- the console alone should fill the dialog.
    let project = Project::new("render-output-no-recap");
    let mut app = app_with_flash(&project);

    app.handle(key(KeyCode::Enter)); // chip information
    settle(&mut app);

    let frame = render(&mut app, 110, 32);
    assert!(!frame.contains("console"), "{frame}");
    assert!(frame.contains("Chip is ESP32"), "{frame}");
}

#[test]
fn the_confirm_overlay_renders_the_literal_command() {
    let project = Project::new("render-confirm");
    let mut app = app_with_flash(&project);
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));

    let frame = render(&mut app, 110, 32);
    assert!(frame.contains("erase-flash"), "{frame}");
}

#[test]
fn the_flash_dialog_is_layered_over_the_dashboard_not_a_replacement_for_it() {
    let project = Project::new("render-dialog-over-dashboard");
    let mut app = app_with_flash(&project);
    let frame = render(&mut app, 110, 32);

    assert!(
        frame.contains("Project") && frame.contains("Device") && frame.contains("Log"),
        "the dashboard panes must stay visible behind the flash dialog:\n{frame}"
    );
    assert!(
        frame.contains("Flash") && frame.contains("Chip information"),
        "the flash menu must render as a dialog on top:\n{frame}"
    );
}

#[test]
fn the_flash_view_survives_a_range_of_sizes() {
    let project = Project::new("render-sizes");
    let mut app = app_with_flash(&project);
    for (width, height) in [(60, 14), (80, 24), (110, 24), (200, 50), (61, 15)] {
        assert!(!render(&mut app, width, height).is_empty());
    }
}

#[test]
fn help_in_the_flash_view_describes_flash_keys() {
    let project = Project::new("help");
    let mut app = app_with_flash(&project);

    let shortcuts: Vec<&str> = app.shortcuts().iter().map(|(key, _)| *key).collect();
    assert!(shortcuts.contains(&"?"), "{shortcuts:?}");

    app.overlay = Some(Overlay::Help {
        filter: String::new(),
        filtering: false,
        selected: 0,
    });
    let frame = render(&mut app, 110, 32);
    assert!(frame.contains("move the menu cursor"), "{frame}");
}

/// An app pointed at both fake tools, with the chip already known so
/// online search has an MCU to search for.
fn app_with_online_search(project: &Project) -> App {
    let mut app = app_with_flash(project);
    let flash = app.flash.as_mut().unwrap();
    flash.set_curl_tool_path(fake_curl());
    // Two presses of `cycle_chip` land on `Esp32` (`ChipFamily::ALL[1]`),
    // matching the fake curl fixture's canned `mcu=esp32` responses.
    flash.cycle_chip(true);
    flash.cycle_chip(true);
    app
}

#[test]
fn searching_online_from_the_menu_lists_boards_then_firmware() {
    let project = Project::new("online-search");
    let mut app = app_with_online_search(&project);

    app.handle(key(KeyCode::Char('s')));
    settle(&mut app);
    assert_eq!(
        app.flash.as_ref().unwrap().screen,
        FlashScreen::OnlineBoards
    );
    assert_eq!(app.flash.as_ref().unwrap().online_boards.len(), 2);

    app.handle(key(KeyCode::Enter)); // ESP32_GENERIC, the first board
    settle(&mut app);
    let flash = app.flash.as_ref().unwrap();
    assert_eq!(flash.screen, FlashScreen::OnlineFirmware);
    assert_eq!(
        flash.online_firmware.len(),
        1,
        "the .app-bin link must not be offered: {:?}",
        flash.online_firmware
    );
}

#[test]
fn downloading_online_firmware_offers_the_erase_and_flash_chain() {
    let project = Project::new("online-download");
    let mut app = app_with_online_search(&project);

    app.handle(key(KeyCode::Char('s')));
    settle(&mut app);
    app.handle(key(KeyCode::Enter)); // pick the board
    settle(&mut app);
    app.handle(key(KeyCode::Enter)); // pick the (only) firmware file
    settle(&mut app);

    // The download landed as a real local file, and its own success chain
    // ran straight into the same confirmed erase-flash the menu uses ---
    // never a silent flash.
    let downloaded = project
        .root
        .join("firmware")
        .join("ESP32_GENERIC-20260406-v1.28.0.bin");
    assert_eq!(
        std::fs::read_to_string(&downloaded).unwrap(),
        "firmware-bytes"
    );
    match &app.overlay {
        Some(Overlay::Confirm { message, .. }) => assert!(
            message.contains("erase-flash"),
            "must show the literal command, not a paraphrase: {message}"
        ),
        other => panic!("expected the erase-flash confirmation, got {other:?}"),
    }

    app.handle(key(KeyCode::Char('y'))); // accept the erase
    settle(&mut app);
    let flash = app.flash.as_ref().unwrap();
    assert!(
        matches!(flash.state, RunState::Succeeded),
        "state was {:?}",
        flash.state
    );
    assert_eq!(
        flash.screen,
        FlashScreen::Options,
        "erase succeeded with one firmware file present, so write-flash is offered next"
    );
    assert_eq!(flash.selected_action(), FlashAction::WriteFlash);
}

#[test]
fn a_pasted_url_downloads_without_searching() {
    let project = Project::new("custom-url");
    let mut app = app_with_online_search(&project);

    app.handle(key(KeyCode::Char('u')));
    assert_eq!(app.flash.as_ref().unwrap().screen, FlashScreen::CustomUrl);

    for c in "https://micropython.org/resources/firmware/ESP32_GENERIC-20260406-v1.28.0.bin".chars()
    {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));
    settle(&mut app);

    let downloaded = project
        .root
        .join("firmware")
        .join("ESP32_GENERIC-20260406-v1.28.0.bin");
    assert!(downloaded.exists());
}

#[test]
fn re_downloading_the_same_file_asks_before_overwriting() {
    let project = Project::new("overwrite");
    let mut app = app_with_online_search(&project);
    let existing = project
        .root
        .join("firmware")
        .join("ESP32_GENERIC-20260406-v1.28.0.bin");
    std::fs::write(&existing, "old contents").unwrap();

    app.handle(key(KeyCode::Char('u')));
    for c in "https://micropython.org/resources/firmware/ESP32_GENERIC-20260406-v1.28.0.bin".chars()
    {
        app.handle(key(KeyCode::Char(c)));
    }
    app.handle(key(KeyCode::Enter));

    match &app.overlay {
        Some(Overlay::ConfirmDownloadOverwrite { dest, .. }) => {
            assert_eq!(dest, &existing);
        }
        other => panic!("expected an overwrite confirmation, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&existing).unwrap(),
        "old contents",
        "nothing must be overwritten before the user confirms"
    );

    app.handle(key(KeyCode::Char('y'))); // confirm the overwrite
    settle(&mut app);
    assert_eq!(
        std::fs::read_to_string(&existing).unwrap(),
        "firmware-bytes"
    );
}

#[test]
fn a_query_with_no_matching_boards_keeps_the_search_window_open() {
    let project = Project::new("online-empty");
    let mut app = app_with_flash(&project);
    let flash = app.flash.as_mut().unwrap();
    flash.set_curl_tool_path(fake_curl());
    flash.cycle_chip(true); // Esp8266 first, then...
    flash.cycle_chip(true); // ...Esp32...
    flash.cycle_chip(true); // ...Esp32S2...
    flash.cycle_chip(true); // ...Esp32S3...
    flash.cycle_chip(true); // ...Esp32C3: the fake curl fixture returns nothing for it.

    app.handle(key(KeyCode::Char('s')));
    assert_eq!(
        app.flash.as_ref().unwrap().screen,
        FlashScreen::OnlineBoards,
        "the search window opens immediately, before any result"
    );
    settle(&mut app);

    let flash = app.flash.as_ref().unwrap();
    assert_eq!(flash.screen, FlashScreen::OnlineBoards);
    assert!(flash.online_boards.is_empty());
    assert!(
        app.logs
            .visible(10)
            .any(|entry| entry.message.contains("no boards found"))
    );
}

fn device(port: &str) -> DeviceInfo {
    DeviceInfo {
        port: port.to_string(),
        serial: None,
        vid_pid: "2e8a:0005".to_string(),
        description: "MicroPython Board".to_string(),
    }
}

/// An app whose browser row exists (so the device pane can host the Project
/// actions tab), pointed at the fake esptool, with `x` already pressed: the
/// tab is showing and holds focus. The device scan runs against the real
/// `mpremote` on PATH (there is none in CI), so discovery simply fails ---
/// the pane and its tab exist regardless.
fn app_in_actions_tab(project: &Project) -> App {
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.handle(key(KeyCode::Char('x')));
    app.flash.as_mut().unwrap().set_tool_path(fake_esptool());
    app
}

/// Entering a MicroPython backend for the first time starts on the Project
/// actions tab, not the files columns: the row is sized for that tab's stack
/// and its buttons are the backend's front door. Both first-entry doors agree
/// --- the startup route's `place_startup_focus` and the empty-project
/// prompt's answer --- and the panel exists from the first frame because no
/// background query creates it while no board is plugged in.
#[test]
fn entering_a_micropython_backend_starts_on_the_actions_tab() {
    let project = Project::new("startup-actions-tab");
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.place_startup_focus();

    assert_eq!(app.focus, Focus::FilesDevice);
    assert!(app.device_actions_tab_active());
    assert!(app.flash.is_some(), "the tab has a panel to draw");

    // The prompt's answer lands there too: the app had no backend before
    // it, so there is no focus worth keeping.
    let project = Project::new("prompt-actions-tab");
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.maybe_open_project_setup();
    assert_eq!(
        app.overlay,
        Some(Overlay::ProjectSetup { selected: 0 }),
        "the empty directory prompts for its backend"
    );
    app.handle(key(KeyCode::Enter)); // MicroPython is the first option
    assert_eq!(app.focus, Focus::FilesDevice);
    assert!(app.device_actions_tab_active());
}

#[test]
fn x_switches_the_device_pane_to_the_actions_tab() {
    let project = Project::new("pane-open");
    let mut app = app_in_actions_tab(&project);

    assert_ne!(app.view, View::Flash, "no dialog opens");
    assert_eq!(app.focus, Focus::FilesDevice);
    assert!(app.device_actions_tab_active());

    // 6 stacked buttons cost 2N+1 rows plus the reserved three-row footer:
    // the row needs 16+ rows of its own, so render tall enough to fit.
    let frame = render(&mut app, 110, 40);
    assert!(
        frame.contains("↯ Actions • ▣ Device Files"),
        "missing the pane's tab strip:\n{frame}"
    );
    assert!(frame.contains("ℹ  Flash information"), "{frame}");
    assert!(frame.contains("⇪  Write / flash firmware"), "{frame}");
    assert!(frame.contains("⌕  Search firmware online"), "{frame}");
    // The chip identity is read in the background of every device
    // selection, and a direct URL is pasted from the search window: neither
    // is a button here.
    assert!(!frame.contains("Chip information"), "{frame}");
    assert!(!frame.contains("Firmware URL"), "{frame}");
    assert!(
        frame.contains("no command yet"),
        "the reserved state line before any run:\n{frame}"
    );
    assert!(
        !frame.contains("■  Stop"),
        "Stop appears only while a command runs:\n{frame}"
    );
}

#[test]
fn tabs_answer_to_the_chord_only_from_either_side() {
    let project = Project::new("pane-switch");
    let mut app = app_in_actions_tab(&project);
    let ctrl = |code: KeyCode| AppEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL));

    // The plain arrows no longer switch any strip: on the actions side the
    // stacked buttons take ↑/↓ alone and ←/→ answer to nothing, and on the
    // files side they keep their directory meaning.
    app.handle(key(KeyCode::Left));
    assert!(
        app.device_actions_tab_active(),
        "a plain ← must not leave the actions tab"
    );
    assert_eq!(app.focus, Focus::FilesDevice);

    // The chord is the tab key: ctrl+→ walks Actions → Device Files,
    // ctrl+← walks back, and the pane keeps the cursor throughout.
    app.handle(ctrl(KeyCode::Right));
    assert!(
        !app.device_actions_tab_active(),
        "ctrl+→ walks onto the Device Files tab"
    );
    assert_eq!(app.focus, Focus::FilesDevice);
    app.handle(ctrl(KeyCode::Left));
    assert!(
        app.device_actions_tab_active(),
        "ctrl+← walks back onto the Actions tab"
    );
    assert_eq!(app.focus, Focus::FilesDevice);

    // One ctrl+← more leaves for pane 3 --- the walk continues past the
    // strip's first tab onto the pane beside it.
    app.handle(ctrl(KeyCode::Left));
    assert_eq!(
        app.focus,
        Focus::FilesLocal,
        "ctrl+← from the Actions tab returns to pane 3"
    );
}

/// The chord's walk over the MicroPython working row, from the local files
/// pane: ctrl+→ steps onto the device pane's first tab (never descending
/// the local directory the way a plain → would), and the tabs walk on from
/// there. One keypress moves one stop: row 3 keeps its place throughout.
#[test]
fn the_strip_chord_reaches_the_device_pane_from_the_local_files_pane() {
    let project = Project::new("chord-from-local");
    let mut app = app_in_actions_tab(&project);

    // Walk off the actions tab onto Device Files, then to pane 3, the same
    // walk a user takes: Actions → Device Files → pane 3.
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Right,
        KeyModifiers::CONTROL,
    )));
    assert!(!app.device_actions_tab_active());
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Left,
        KeyModifiers::CONTROL,
    )));
    assert!(app.device_actions_tab_active());
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Left,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.focus, Focus::FilesLocal);
    assert_eq!(
        app.browser.as_ref().unwrap().local_path,
        project.root,
        "the walk never descends a local directory"
    );
    assert_eq!(
        app.log_tab,
        LogTab::Log,
        "one keypress, one stop: row 3 is untouched"
    );

    // And from pane 3 the walk re-enters at the strip's first tab.
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Right,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.focus, Focus::FilesDevice);
    assert!(app.device_actions_tab_active());
    assert_eq!(app.log_tab, LogTab::Log);
}

/// The device pane's two tabs share one height: the row is sized to the
/// actions tab's button stack whenever the strip exists, so flipping
/// between Files and Actions (by chord from anywhere) must not reflow the
/// rows below --- row 3's strip stays exactly where it was.
#[test]
fn the_two_device_tabs_hold_the_same_row_height() {
    let project = Project::new("pane-stable-height");
    let mut app = app_in_actions_tab(&project);

    let strip_rows = |app: &mut App| -> (usize, usize) {
        let frame = render(app, 110, 40);
        let device = frame
            .lines()
            .position(|line| line.contains("↯ Actions • ▣ Device Files"))
            .expect("the device pane's strip");
        let log = frame
            .lines()
            .position(|line| line.contains("▤ Log • ◉ Monitor"))
            .expect("row 3's strip");
        (device, log)
    };

    let actions = strip_rows(&mut app);
    app.handle(key(KeyCode::Left)); // plain ← from actions: over to the files tab
    let files = strip_rows(&mut app);
    assert_eq!(
        actions, files,
        "switching the tabs must not move the device pane or the rows below it"
    );
}

/// The strip sizes row 2 from the moment it exists, with no flash panel
/// yet: the row must already hold the height the Actions tab's stack will
/// need, so the first entry onto the tab (`x`) draws in place instead of
/// snapping the whole dashboard taller.
#[test]
fn the_row_holds_the_stack_height_before_the_panel_exists() {
    let project = Project::new("pane-height-before-panel");
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();

    let strip_to_log = |app: &mut App| -> usize {
        let frame = render(app, 110, 40);
        let device = frame
            .lines()
            .position(|line| line.contains("↯ Actions • ▣ Device Files"))
            .expect("the device pane's strip");
        let log = frame
            .lines()
            .position(|line| line.contains("▤ Log • ◉ Monitor"))
            .expect("row 3's strip");
        log - device
    };

    let before = strip_to_log(&mut app);
    app.handle(key(KeyCode::Char('x')));
    let after = strip_to_log(&mut app);
    assert_eq!(
        before, after,
        "entering the Actions tab must not change the row's height"
    );
}

#[test]
fn the_actions_tab_keeps_focus_and_parks_the_cursor_on_stop() {
    let project = Project::new("pane-run");
    let mut app = app_in_actions_tab(&project);

    for _ in 0..3 {
        // Search online, Manage packages, Flash information, then Reset:
        // the row `Verify flash` gave up is `Manage packages`, so the
        // read-only run this test wants sits one lower than it did.
        app.handle(key(KeyCode::Down)); // Reset, read-only
    }
    app.handle(key(KeyCode::Enter));
    let flash = app.flash.as_ref().unwrap();
    assert!(flash.is_busy(), "the action started");
    assert_eq!(
        flash.pane_cursor,
        flash.pane_actions().len() - 1,
        "the cursor parks on Stop while the command runs"
    );
    // The build pane's rule: the Monitor tab is shown, never focused --- the
    // pane keeps the user while the command runs.
    assert_eq!(app.log_tab, LogTab::Monitor);
    assert_eq!(app.monitor_source, MonitorSource::Flash);
    assert_eq!(app.view, View::Dashboard);
    assert_eq!(app.focus, Focus::FilesDevice);
    let frame = render(&mut app, 110, 40);
    assert!(frame.contains("■  Stop"), "{frame}");

    settle(&mut app);
    assert!(
        app.device_actions_tab_active(),
        "the tab survives its own run"
    );
    assert_eq!(app.focus, Focus::FilesDevice);
    assert_eq!(
        app.flash.as_ref().unwrap().pane_cursor,
        3,
        "a finished command lands back on its own row"
    );
    let frame = render(&mut app, 110, 40);
    assert!(
        frame.contains("Reset ok in"),
        "the report line names the finished run:\n{frame}"
    );
    assert!(!frame.contains("■  Stop"), "{frame}");
}

#[test]
fn erase_from_the_actions_tab_still_confirms_with_the_literal_command() {
    let project = Project::new("pane-erase");
    let mut app = app_in_actions_tab(&project);

    for _ in 0..4 {
        app.handle(key(KeyCode::Down)); // Erase flash
    }
    app.handle(key(KeyCode::Enter));

    match &app.overlay {
        Some(Overlay::Confirm { message, .. }) => assert!(
            message.contains("erase-flash"),
            "the confirmation must show the real command: {message}"
        ),
        other => panic!("expected the erase confirmation, got {other:?}"),
    }
    assert!(
        !app.flash.as_ref().unwrap().is_busy(),
        "nothing runs before the user accepts"
    );

    app.handle(key(KeyCode::Char('y')));
    settle(&mut app);
    assert!(
        matches!(app.flash.as_ref().unwrap().state, RunState::Succeeded),
        "the erase ran after the accept"
    );
}

#[test]
fn the_search_button_opens_the_online_window_as_a_dialog() {
    let project = Project::new("pane-search");
    let mut app = app_in_actions_tab(&project);
    {
        let flash = app.flash.as_mut().unwrap();
        flash.set_curl_tool_path(fake_curl());
        flash.cycle_chip(true);
        flash.cycle_chip(true); // Esp32, matching the curl fixture
    }
    // Search firmware online --- the tab's first row
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.view, View::Flash, "the online window is a dialog");
    assert_eq!(
        app.flash.as_ref().unwrap().screen,
        FlashScreen::OnlineBoards
    );
    settle(&mut app);
    assert_eq!(
        app.flash.as_ref().unwrap().online_boards.len(),
        2,
        "the fixture's two boards arrive"
    );
}

#[test]
fn a_device_becoming_known_at_startup_queries_it_with_esptool_in_the_background() {
    // Mirrors how `App::maybe_scan_devices` finds an mpremote scan already
    // resolved to a single board: the device panel should not need the user
    // to open the Flash view by hand to learn what is connected --- but the
    // reading restarts the board, so it asks first (default No).
    let project = Project::new("auto-query-startup");
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));

    assert!(
        app.flash.is_none(),
        "nothing queried before a device exists"
    );
    app.devices.set_devices(vec![device("/dev/ttyACM0")]);
    assert_eq!(
        app.devices.selected_port(),
        Some("/dev/ttyACM0"),
        "a lone device selects itself"
    );

    // The picker overlay is the one place `App` already exercises the
    // device-selection path without a real mpremote process (`apply_device_picker`
    // reruns the same selection logic `on_process` does after a real scan).
    app.overlay = Some(Overlay::DevicePicker { selected: 0 });
    app.handle(key(KeyCode::Enter));

    // The question opens on the next tick and gates the query: nothing
    // reads the board before the answer.
    app.handle(AppEvent::Tick);
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmIdentifyDevice { .. })
    ));
    assert!(
        !app.flash.as_ref().is_some_and(|flash| flash.is_busy()),
        "no query before the answer"
    );
    app.handle(key(KeyCode::Char('y')));
    assert!(
        app.flash
            .as_ref()
            .is_some_and(|flash| flash.is_busy() && flash.screen == FlashScreen::Menu),
        "the accepted answer must kick off a background flash-id query without \
         opening the Flash view"
    );
}

#[test]
fn switching_devices_from_the_picker_re_queries_the_newly_selected_one() {
    let project = Project::new("auto-query-switch");
    let mut app = app_with_flash(&project);

    app.devices
        .set_devices(vec![device("/dev/ttyACM0"), device("/dev/ttyACM1")]);
    app.overlay = Some(Overlay::DevicePicker { selected: 1 });
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.devices.selected_port(), Some("/dev/ttyACM1"));
    // The identification question (the pick's follow-through now) opens on
    // the next tick --- no probe or listing exists to release it here ---
    // and answering yes is what re-queries the newly selected board.
    app.handle(AppEvent::Tick);
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmIdentifyDevice { .. })
    ));
    assert!(
        !app.flash.as_ref().is_some_and(|flash| flash.is_busy()),
        "nothing reads the board before the answer"
    );
    app.handle(key(KeyCode::Char('y')));
    assert!(
        app.flash.as_ref().is_some_and(|flash| flash.is_busy()),
        "switching devices must re-query the newly selected one"
    );
}

#[test]
fn picking_a_device_defers_the_esptool_query_until_mpremote_releases_the_port() {
    // Regression test: `esptool` and `mpremote` both hold the serial port
    // exclusively. Picking a device also kicks off an `mpremote fs ls` for
    // the file browser, so firing the background chip/flash query in the
    // same instant used to make esptool lose the race for the port
    // ("cannot open the serial port") every time.
    let project = Project::new("defer-query");
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));

    // Stand in for what `ensure_browser_scanning`/`ensure_flash_panel` would
    // otherwise lazily create pointed at the real `mpremote`/`esptool` on
    // PATH --- pre-seeding lets both subsystems be driven with real
    // processes without needing either tool installed.
    let mut browser = Browser::new(&project.root);
    browser.set_tool_path(fake_mpremote());
    app.browser = Some(browser);
    let mut flash = FlashPanel::new(&project.root);
    flash.set_tool_path(fake_esptool());
    app.flash = Some(flash);

    app.devices.set_devices(vec![device("/dev/ttyACM0")]);
    app.overlay = Some(Overlay::DevicePicker { selected: 0 });
    app.handle(key(KeyCode::Enter)); // apply_device_picker

    // The device-script probe now owns the port before the listing (see
    // `app::probe`); whichever mpremote session runs first, the invariant
    // under test is the same: esptool must not race it for the port.
    assert!(
        !app.flash.as_ref().unwrap().is_busy(),
        "esptool must not race mpremote for the port"
    );

    // Drive everything to completion: probe, the identification question
    // its release opens, then (once answered) the chip identity query, then
    // the firmware read its success arms (the new order --- the
    // identification gates the first listing), then the listing its verdict
    // releases. The loop breaks only when every tool is done.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        if matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })) {
            app.handle(key(KeyCode::Char('y')));
        }
        if !app.browser.as_ref().unwrap().is_busy()
            && app.flash.as_ref().is_some_and(|flash| {
                flash.details.family.is_some()
                    && flash.details.firmware.is_some()
                    && !flash.is_busy()
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "probe, listing or the deferred esptool queries never finished"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "mpremote never finished"
    );
    assert!(
        !app.flash.as_ref().unwrap().is_busy(),
        "the deferred esptool queries never ran"
    );
    assert_eq!(
        app.flash.as_ref().unwrap().details.family,
        Some(ChipFamily::Esp32),
        "the deferred query must still have reached esptool and parsed its reply"
    );
}

#[test]
fn a_direct_url_can_be_pasted_from_the_boards_window() {
    // The window's own hint points at 'u'; it must work there, not only
    // from the menu.
    let project = Project::new("url-from-boards");
    let mut app = app_with_online_search(&project);

    app.handle(key(KeyCode::Char('s')));
    settle(&mut app);
    assert_eq!(
        app.flash.as_ref().unwrap().screen,
        FlashScreen::OnlineBoards
    );

    app.handle(key(KeyCode::Char('u')));
    assert_eq!(app.flash.as_ref().unwrap().screen, FlashScreen::CustomUrl);
}

#[test]
fn leaving_a_dialog_returns_to_the_pane_that_hosts_the_menu() {
    // The actions tab *is* the menu, so `esc` out of a dialog screen must
    // land on the dashboard --- stepping back to `FlashScreen::Menu` would
    // draw the six actions a second time, over the pane already showing
    // them, with the dialog's own footer to match.
    let project = Project::new("pane-esc");
    let mut app = app_in_actions_tab(&project);
    {
        let flash = app.flash.as_mut().unwrap();
        flash.set_curl_tool_path(fake_curl());
        flash.cycle_chip(true);
        flash.cycle_chip(true); // Esp32, matching the curl fixture
    }

    // Search firmware online --- the tab's first row
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.view, View::Flash);
    assert_eq!(
        app.flash.as_ref().unwrap().screen,
        FlashScreen::OnlineBoards
    );

    app.handle(key(KeyCode::Esc));
    assert_eq!(app.view, View::Dashboard, "back is the pane, not a menu");
    assert!(app.device_actions_tab_active(), "on the tab it came from");
    let frame = render(&mut app, 110, 40);
    assert!(
        !frame.contains("╭ Flash "),
        "no dialog is left layered over the pane:\n{frame}"
    );
}

#[test]
fn a_refused_search_opens_no_dialog_from_the_tab() {
    // No chip is known here (nothing ever queried the board), so the search
    // only warns. The panel stays on `FlashScreen::Menu` --- which is the
    // pane itself --- and nothing should pop over the dashboard.
    let project = Project::new("pane-search-refused");
    let mut app = app_in_actions_tab(&project);

    // Search firmware online --- the tab's first row
    app.handle(key(KeyCode::Enter));

    assert_eq!(
        app.view,
        View::Dashboard,
        "the log line is the whole answer"
    );
    let frame = render(&mut app, 110, 40);
    assert!(!frame.contains("╭ Flash "), "{frame}");
}

#[test]
fn the_arrows_create_the_flash_panel_the_tab_draws() {
    // `x` is not the only way onto the tab: with no board plugged in
    // nothing else creates the panel, so the strip's own key must. The
    // ctrl chord no longer plays that role (it steps focus between panes
    // now), but `x` is dashboard-wide and creates the panel from whichever
    // pane holds the cursor --- including the device pane's files side,
    // where the plain arrows navigate directories.
    let project = Project::new("pane-arrow-first");
    let mut app = hermetic_app(&project.root);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    app.maybe_scan_devices();
    app.focus = Focus::FilesDevice;
    assert!(app.flash.is_none(), "nothing has created a panel yet");

    // The chord steps focus instead: from the rightmost pane, ctrl+→ has
    // nowhere to go and must not create anything.
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Right,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.focus, Focus::FilesDevice);
    assert!(app.flash.is_none(), "the chord creates no panel");

    app.handle(key(KeyCode::Char('x')));
    assert!(app.device_actions_tab_active());
    assert!(app.flash.is_some(), "the tab arrives with its panel");

    // The row is sized to the button stack, so the buttons are actually
    // there to press --- a panel-less tab collapsed row 2 to its borders.
    let frame = render(&mut app, 110, 40);
    assert!(frame.contains("ℹ  Flash information"), "{frame}");
    assert!(frame.contains("⌕  Search firmware online"), "{frame}");
    assert!(frame.contains("no command yet"), "{frame}");
}

#[test]
fn the_tab_names_a_running_fetch_and_stops_it() {
    // A fetch dims every button on the tab, so the state line has to say
    // what for --- and the `Stop` that appears with it has to reach the
    // fetch, not just the esptool command.
    let project = Project::new("pane-fetch-stop");
    let mut app = app_in_actions_tab(&project);
    {
        let flash = app.flash.as_mut().unwrap();
        flash.set_curl_tool_path(fake_curl());
        flash.cycle_chip(true);
        flash.cycle_chip(true); // Esp32, matching the curl fixture
    }
    // Search firmware online --- the tab's first row
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Esc)); // back to the pane, fetch still in flight

    assert_eq!(app.view, View::Dashboard);
    let frame = render(&mut app, 110, 40);
    assert!(
        frame.contains("searching online…"),
        "the dimmed buttons say what they are waiting for:\n{frame}"
    );
    assert!(frame.contains("■  Stop"), "{frame}");

    app.handle(key(KeyCode::End)); // the Stop row
    app.handle(key(KeyCode::Enter));
    settle(&mut app);
    assert!(
        !app.flash.as_ref().unwrap().is_busy(),
        "the Stop reached the fetch"
    );
    let frame = render(&mut app, 110, 40);
    assert!(!frame.contains("■  Stop"), "{frame}");
}

#[test]
fn the_strip_carries_each_tabs_own_status() {
    // The right edge of the strip belongs to the tab that is showing: the
    // files tab reports where the listing is, the actions tab has no
    // listing to locate --- but a running script gates every esptool
    // action, so that half of the status stays on both.
    let project = Project::new("pane-strip-status");
    let mut app = app_in_actions_tab(&project);
    app.devices.set_devices(vec![device("/dev/ttyACM0")]);
    app.devices.set_script_state(ScriptState::Running);
    // A walked path: the root needs no locating (its lone `/` would read as
    // a stray mark), so the status only carries a path once one is walked.
    app.browser.as_mut().unwrap().device_path = chiptui::device::DevicePath::new("/lib");

    let actions = render(&mut app, 110, 40);
    let strip = actions
        .lines()
        .find(|line| line.contains("↯ Actions • ▣ Device Files"))
        .unwrap()
        .to_string();
    assert!(strip.contains("script running"), "{strip}");
    assert!(
        !strip.contains(" /lib"),
        "no device path to report on this tab: {strip}"
    );

    // Over to the files tab: the chord walks Actions -> Device Files (the
    // plain arrows no longer switch any strip).
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Right,
        KeyModifiers::CONTROL,
    )));
    let files = render(&mut app, 110, 40);
    let strip = files
        .lines()
        .find(|line| line.contains("↯ Actions • ▣ Device Files"))
        .unwrap()
        .to_string();
    assert!(
        strip.contains("/lib · script running"),
        "the walked path comes back with it: {strip}"
    );
}

#[test]
fn the_actions_tab_offers_packages_where_verify_used_to_sit() {
    let project = Project::new("pane-packages");
    let mut app = app_in_actions_tab(&project);

    let frame = render(&mut app, 110, 40);
    assert!(
        frame.contains("Manage packages"),
        "the package manager is a row of the stack:\n{frame}"
    );
    assert!(
        !frame.contains("Verify flash"),
        "and verify no longer spends one:\n{frame}"
    );

    // The swap is deliberately height-neutral: `row2_content_height`'s
    // no-panel fallback is `FlashAction::ALL.len()`, so the row would
    // reflow the moment the panel appeared if these two disagreed --- and
    // the declared 80x32 minimum is measured against the same number.
    let idle = app.flash.as_ref().unwrap().pane_actions().len();
    assert_eq!(
        idle,
        chiptui::flash::FlashAction::ALL.len(),
        "six idle rows, before and after"
    );

    // Enter on the row opens the manager.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, Some(Overlay::Packages));
}

#[test]
fn v_verifies_the_flash_from_the_actions_tab() {
    let project = Project::new("pane-verify");
    let mut app = app_in_actions_tab(&project);

    // No firmware file in the project, so the action is refused rather than
    // run --- but it is *reached*, which is the point: the key replaces the
    // row it used to have.
    app.handle(key(KeyCode::Char('v')));
    assert!(
        app.logs
            .visible(50)
            .any(|entry| entry.message.to_lowercase().contains("firmware")),
        "verify was dispatched and answered for itself"
    );
}
