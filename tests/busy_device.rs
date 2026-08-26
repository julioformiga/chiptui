//! The busy-script paths end to end, against the fake `mpremote` variants.
//!
//! A board whose `main.py` sits in a blocking loop cannot be listed by
//! `mpremote` without interrupting it (Ctrl-C, then raw REPL), so ChipTUI
//! probes first, asks before interrupting, and offers to restart the script
//! afterwards --- the flows exercised here with an idle, a printing and a
//! silent fake board. The identification chain (esptool reading the chip
//! and firmware, which *restarts* the board) asks too, before anything
//! touches the port.

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::app::{App, Focus, Overlay};
use chiptui::backend::BackendKind;
use chiptui::browser::{Browser, PaneState};
use chiptui::device::{DeviceInfo, ScriptState};
use chiptui::event::AppEvent;
use chiptui::firmware_id::{FirmwareVerdict, FlashFirmware};
use chiptui::flash::{FlashAction, FlashPanel};
use chiptui::process::ProcessManager;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn fake(tool: &str) -> String {
    format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
}

/// An app with a MicroPython override, a browser pointed at a fake mpremote,
/// and a fake esptool so the background device query stays deterministic.
fn app_with(tool: &str) -> App {
    app_with_tools(tool, "esptool")
}

/// [`app_with`] with the esptool fake chosen separately --- the ordering
/// tests want a deliberately slow `chip-id` so the window where the first
/// listing is held behind the identity query is observable.
fn app_with_tools(mpremote_tool: &str, esptool_tool: &str) -> App {
    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    let mut browser = Browser::new(std::env::temp_dir());
    browser.set_tool_path(fake(mpremote_tool));
    app.browser = Some(browser);
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.set_tool_path(fake(esptool_tool));
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

/// Pumps until the identification question opens, then answers it: `y`
/// accepts (the chain runs), a reflex `Enter` declines (the default --- the
/// board is left untouched). Every device-selection chain starts with this
/// question now; nothing reads the board before it is answered.
fn answer_identify(app: &mut App, accept: bool) -> bool {
    if !pump_until(
        app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20,
    ) {
        return false;
    }
    app.handle(key(if accept {
        KeyCode::Char('y')
    } else {
        KeyCode::Enter
    }));
    true
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
fn the_identify_interrupt_and_restore_prompts_render() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));

    let frame = render(&mut app, 110, 32);
    assert!(
        frame.contains("A device is connected"),
        "the question must say a device is connected:\n{frame}"
    );
    assert!(
        frame.contains("restarts the board"),
        "the cost of identifying must be named:\n{frame}"
    );

    // Declining leaves the listing to mpremote, which the running script
    // gates behind its own question.
    app.handle(key(KeyCode::Enter));
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));
    let frame = render(&mut app, 110, 32);
    assert!(
        frame.contains("A script is running"),
        "the confirmation should say why it is asking:\n{frame}"
    );
    assert!(
        frame.contains("interrupts"),
        "the mechanism should be named:\n{frame}"
    );

    app.overlay = Some(Overlay::RestoreDeviceScript {
        selected: 2,
        return_to_packages: false,
    });
    let frame = render(&mut app, 110, 32);
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
fn an_idle_board_is_listed_once_the_identification_is_accepted() {
    let mut app = app_with("mpremote");

    // Probe sees the banner and prompt, concludes "idle"; the question
    // opens, the accepted answer runs the chip identity and the firmware
    // read on the same port, and the MicroPython verdict releases the
    // listing.
    assert!(answer_identify(&mut app, true));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
    assert_eq!(app.devices.script_state(), ScriptState::Stopped);
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::MicroPython,
            Some("v1.28.0".to_string())
        ))
    );
    // The one question the chain needed was the identification itself: no
    // interrupt confirm follows an idle board.
    assert_eq!(app.overlay, None, "no interrupt confirm for an idle board");
}

#[test]
fn the_firmware_is_identified_before_the_first_listing() {
    let mut app = app_with("mpremote");

    // The identification read runs as part of the chain the accepted
    // question started, and the MicroPython verdict is what releases the
    // listing.
    assert!(answer_identify(&mut app, true));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        20
    ));
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::MicroPython,
            Some("v1.28.0".to_string())
        ))
    );
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
    let frame = render(&mut app, 110, 32);
    assert!(
        frame.contains("Firmware:  MicroPython v1.28.0"),
        "the pane must name the identified firmware with its version:\n{frame}"
    );

    // The read is once per port: nothing re-opens or re-asks anything.
    assert!(pump_until(&mut app, |app| app.overlay.is_none(), 5));
}

/// Declining the identification is not final for the session: `r` on the
/// device pane is the documented way back, and it re-opens the question
/// (never runs the chain on its own).
#[test]
fn r_offers_the_declined_identification_again() {
    let mut app = app_with("mpremote");
    assert!(answer_identify(&mut app, false));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));

    app.focus = Focus::FilesDevice;
    app.handle(key(KeyCode::Char('r')));
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    // Accepting this time runs the chain the first answer skipped.
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        20
    ));
}

/// `ctrl+r` is the dashboard-wide form of the offer: from any pane (here
/// the Log pane, where plain `r` means re-detect) the chord re-opens the
/// identification question, and accepting runs the chain the earlier
/// decline skipped.
#[test]
fn ctrl_r_offers_the_identification_from_any_pane() {
    let mut app = app_with("mpremote");
    assert!(answer_identify(&mut app, false));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));

    app.focus = Focus::Logs;
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
    assert!(
        matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        "the chord must open the identification question from any pane"
    );

    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        20
    ));
}

/// `Enter` on the Device info pane is the in-pane twin of the chord: with
/// nothing to show it offers the identification (the pane's message names
/// both gestures), and once the data exists it goes back to copying the MAC.
#[test]
fn enter_on_the_device_info_pane_offers_the_identification() {
    let mut app = app_with("mpremote");
    assert!(answer_identify(&mut app, false));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));

    // The pane's empty state names the way forward instead of shrugging.
    let frame = render(&mut app, 110, 32);
    assert!(
        frame.contains("device connected --- not identified"),
        "the empty pane must say what is going on:\n{frame}"
    );
    assert!(
        frame.contains("ctrl+r, or Enter here, stops it and reads its data"),
        "the shortcut must be in the message:\n{frame}"
    );

    app.focus = Focus::DeviceInfo;
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        "Enter on the empty pane must offer the identification"
    );

    // Accepting captures the data; a later Enter, with a MAC to act on,
    // copies it instead of re-asking.
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.mac.is_some()),
        20
    ));
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, None, "data read: Enter copies, not asks");
    assert!(
        app.take_clipboard_request()
            .is_some_and(|text| text.contains(':')),
        "the MAC was copied"
    );
}

/// A write-flash rewrites the board, so the verdict the identification read
/// produced is stale the moment the write finishes: like `west flash` on
/// the Zephyr side, the identification re-arms and re-runs on its own once
/// the port frees --- the pane must not sit on the old answer (or on
/// `undefined`) until a manual `r` or a device reselect.
#[test]
fn a_write_flash_re_identifies_the_firmware_on_its_own() {
    // One firmware candidate, so the panel has something to write without
    // walking the options screen.
    let dir = std::env::temp_dir().join(format!("chiptui-rewrite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.bin"), b"x").unwrap();

    let mut app = App::new(std::env::temp_dir());
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    let mut browser = Browser::new(std::env::temp_dir());
    browser.set_tool_path(fake("mpremote"));
    app.browser = Some(browser);
    let mut flash = FlashPanel::new(&dir);
    flash.set_tool_path(fake("esptool"));
    app.flash = Some(flash);
    app.maybe_scan_devices();
    app.handle(key(KeyCode::Char('d')));

    // The selection chain asks first; the accepted answer answers it:
    // MicroPython v1.28.0.
    assert!(answer_identify(&mut app, true));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        20
    ));
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::MicroPython,
            Some("v1.28.0".to_string())
        ))
    );

    // Write a firmware over it, armed the way the write flow leaves the
    // panel (file chosen, offset typed).
    let port = app.devices.selected_port().unwrap().to_string();
    let Some(mut flash) = app.flash.take() else {
        unreachable!("the flash panel exists");
    };
    flash.discover_firmware();
    assert!(flash.select_firmware(0), "the lone firmware is selectable");
    flash.set_offset("0x1000".to_string());
    let notices = flash.run(FlashAction::WriteFlash, &mut app.processes, Some(&port));
    app.flash = Some(flash);
    assert!(notices.is_empty(), "the write started: {notices:?}");

    // The write is the only user-started command, so its report marks the
    // finish. From there the verdict must come back with no user action at
    // all: no `r`, no reselect, no re-opened browser.
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .and_then(|flash| flash.last.as_ref())
            .is_some_and(|report| report.ok),
        20
    ));
    assert!(
        app.logs
            .visible(100)
            .any(|entry| entry.message.contains("re-identifying its firmware")),
        "the reload must say what it is doing"
    );
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        20
    ));
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::MicroPython,
            Some("v1.28.0".to_string())
        )),
        "the read really ran again after the write"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_board_running_zephyr_is_refused_rather_than_listed() {
    // The chip is an ordinary ESP32; only the firmware differs. The
    // accepted identification names it, and the listing is refused with
    // the reason instead of garbage-listing a board mpremote cannot talk
    // to.
    let mut app = app_with_tools("mpremote", "esptool-zephyr-board");

    assert!(answer_identify(&mut app, true));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Failed(_)),
        20
    ));
    assert!(matches!(
        pane(&app),
        PaneState::Failed(message)
            if message.contains("Zephyr") && message.contains("cannot read files")
    ));
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "no listing was attempted against the Zephyr firmware"
    );
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::Zephyr,
            Some("v4.0.0".to_string())
        ))
    );
    let frame = render(&mut app, 110, 32);
    assert!(
        frame.contains("not MicroPython"),
        "the pane must carry the warning:\n{frame}"
    );

    // 'r' is the recovery path: it re-runs the identification (a re-flash
    // may have changed the answer) and, the board still running Zephyr,
    // refuses again.
    app.focus = Focus::FilesDevice;
    app.handle(key(KeyCode::Char('r')));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Failed(_)),
        20
    ));
    assert!(
        matches!(pane(&app), PaneState::Failed(message) if message.contains("Zephyr")),
        "the re-check must refuse again with the reason"
    );
}

#[test]
fn a_busy_board_holds_the_listing_behind_a_confirmation() {
    let mut app = app_with("mpremote-busy-board");

    // The probe found the running script; the identification question is
    // what opens (its yes accepts the interruption the reading causes).
    assert!(
        pump_until(
            &mut app,
            |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
            20
        ),
        "the probe found the running script and the question opened"
    );
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "nothing was sent to the port: the listing is held behind the identification chain, not queued in the browser"
    );
    assert!(
        matches!(pane(&app), PaneState::Loading),
        "the pane is waiting, not failed"
    );
    assert_eq!(app.devices.script_state(), ScriptState::Running);
}

#[test]
fn a_banner_printing_foreign_board_identifies_before_any_listing() {
    // A board whose firmware prints its boot banner when the port opens
    // (any foreign firmware on an auto-reset ESP32): the probe cannot tell
    // it from a busy script, so the identification question opens with the
    // script belief riding it --- and what the accepted answer must move
    // is the identification chain, never a file listing. Only the verdict
    // may refuse or release the listing.
    let mut app = app_with_tools("mpremote-zephyr-board", "esptool-zephyr-board");

    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "no mpremote filesystem command may run before the firmware is known"
    );

    app.handle(key(KeyCode::Char('y')));

    // The chain runs: chip identity, then the firmware read --- and the
    // Zephyr verdict refuses the listing with the reason instead of ever
    // talking to a filesystem the firmware does not have.
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Failed(_)),
        20
    ));
    assert!(
        matches!(pane(&app), PaneState::Failed(message) if message.contains("Zephyr"),),
        "the refusal must name the firmware"
    );
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "the listing was refused, not attempted"
    );
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::Zephyr,
            Some("v4.0.0".to_string())
        ))
    );
    // There was no MicroPython script to bring back: the "running script"
    // was the firmware's own boot banner, so no restore question opens.
    assert!(pump_until(&mut app, |app| app.overlay.is_none(), 5));
}

#[test]
fn declining_the_interruption_drops_the_listing() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));

    // Decline the identification (default No): the board is not restarted,
    // and the listing it releases is mpremote's to make --- which the
    // running script gates behind the interrupt question.
    app.handle(key(KeyCode::Enter));
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));

    // Default is No here too; a reflex Enter must not stop the board's
    // script.
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, None);
    assert!(matches!(pane(&app), PaneState::Failed(message) if message.contains("script")));
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "the held listing was dropped, not left loading forever"
    );
    assert_eq!(
        app.devices.script_state(),
        ScriptState::Running,
        "declining leaves the script alone"
    );
}

#[test]
fn accepting_the_identification_on_a_busy_board_lists_and_offers_restore() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));

    // One yes covers both the restart the reading causes and the script it
    // stops: the question names both, so no second interrupt confirm
    // follows it.
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
            Some(Overlay::RestoreDeviceScript { selected: 2, .. })
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
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| matches!(
            app.overlay,
            Some(Overlay::RestoreDeviceScript { selected: 2, .. })
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

    // Nothing to see: the probe's window passes, it gives up, and the
    // identification question opens anyway --- Unknown is not Running, so
    // the accepted answer runs the chain ungated (the documented blind
    // spot for scripts that never print).
    assert!(answer_identify(&mut app, true));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
    assert_eq!(app.devices.script_state(), ScriptState::Unknown);
    // The identification answers MicroPython with no further overlay to
    // answer along the way.
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::MicroPython,
            Some("v1.28.0".to_string())
        ))
    );
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
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));

    // The restore prompt opens once the accepted operations drain --- the
    // chain now ends with the /lib coverage listing, so it lands a moment
    // after the pane itself is ready. The esptool query must not fire
    // underneath a decision that may also want the port (a reset, say).
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::RestoreDeviceScript { .. })),
        20
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
fn a_declined_identification_never_touches_the_board() {
    let mut app = app_with("mpremote-busy-board");
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));

    // Decline the identification (default No + Enter): the user said "do
    // not restart the board", and esptool resets it to read anything, so
    // nothing may run. The listing the decline releases is mpremote's,
    // gated behind the interrupt question --- declined too.
    app.handle(key(KeyCode::Enter));
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmInterruptDevice { .. })),
        20
    ));
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

    // Even once the script is no longer believed running, the refused
    // identification stays refused --- declining is an answer about this
    // port, not a postponement.
    app.devices.set_script_state(ScriptState::Stopped);
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        app.handle(AppEvent::Tick);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        app.flash.as_ref().unwrap().details.family.is_none(),
        "the declined identification must not run on its own"
    );
}

#[test]
fn the_first_listing_waits_for_the_chip_identity_query() {
    // The slow fake esptool (~1s chip-id and read-flash) widens the window
    // in which each identification step owns the port, so the ordering is
    // observable: chip-id first, the firmware read next, files only after
    // its verdict.
    let mut app = app_with_tools("mpremote", "esptool-slow-chip");
    assert!(answer_identify(&mut app, true));

    // The probe found the board idle, the answer authorized the chain, and
    // the identity query is the next tool on the port, not the listing.
    assert!(
        pump_until(
            &mut app,
            |app| app.flash.as_ref().is_some_and(|flash| flash.is_busy()),
            20
        ),
        "the chip query should start once the probe releases the port"
    );
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "the listing must not race the chip query for the serial port"
    );
    assert!(
        matches!(pane(&app), PaneState::Loading),
        "the pane waits rather than failing"
    );

    // The chip query's success arms the firmware read --- the listing waits
    // behind that too now.
    assert!(
        pump_until(
            &mut app,
            |app| app
                .flash
                .as_ref()
                .is_some_and(|flash| flash.details.family.is_some()),
            20
        ),
        "the chip identity should arrive"
    );
    assert!(
        pump_until(
            &mut app,
            |app| app.flash.as_ref().is_some_and(|flash| flash.is_busy()),
            20
        ),
        "the firmware read should start once the chip query releases the port"
    );
    assert!(
        !app.browser.as_ref().unwrap().is_busy(),
        "the listing must not race the firmware read either"
    );
    assert!(matches!(pane(&app), PaneState::Loading));

    // The read's verdict is the listing's cue; the dashboard ends up with
    // the chip identity, the firmware and the files.
    assert!(pump_until(
        &mut app,
        |app| matches!(pane(app), PaneState::Ready),
        20
    ));
    assert!(app.flash.as_ref().unwrap().details.family.is_some());
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::MicroPython,
            Some("v1.28.0".to_string())
        ))
    );
}
