//! The Zephyr monitor end to end: USB serial port discovery without
//! mpremote, the device picker for several ports, and the platform's own
//! monitor --- `west espressif monitor -p PORT`, the west extension the
//! workspace ships for ESP32 boards (there is no `west monitor`) --- in a
//! PTY session (`SPEC.md` §10's monitor, wired to the Monitor tab), with
//! every missing fact refused by name.

#![cfg(unix)]

use chiptui::app::{App, Focus, LogTab, MonitorSource, Overlay};
use chiptui::backend::BackendKind;
use chiptui::backend::zephyr::workspace::{Resolution, Workspace, WorkspaceOrigin};
use chiptui::device::DiscoveryState;
use chiptui::firmware_id::{FirmwareVerdict, FlashFirmware};
use chiptui::flash::FlashPanel;
use chiptui::workspace::WorkspacePanel;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{fake, key, pump_until, render};

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
    app.set_serial_dir(root.join("dev"));
    // Workspace discovery must not look at the machine's real $HOME (a
    // ~/zephyrproject there would resolve the pane differently) --- set
    // before `bootstrap`, whose tool report already resolves the workspace.
    std::fs::create_dir_all(root.join("home")).unwrap();
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    // Pre-seeded so the background chip identity query a selection defers
    // runs against the fake, not whatever `esptool` happens to be on the
    // machine running the tests.
    let mut flash = FlashPanel::new(&root);
    flash.set_tool_path(fake("esptool"));
    app.flash = Some(flash);
    app.maybe_scan_devices();
    // After the scan: `maybe_scan_devices` is what creates the build panel
    // the override belongs to.
    if let Some(panel) = app.build.as_mut() {
        panel.set_tool_path(fake("west"));
    }
    (app, root)
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
    // there is no filesystem to list and no probe on this backend; the pick
    // itself defers only the chip identity query (started by the next tick,
    // so nothing is in flight in this instant).
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    let expected = root.join("dev/ttyACM1").display().to_string();
    assert_eq!(
        app.devices.selected().map(|d| d.port.clone()),
        Some(expected)
    );
    assert!(
        app.processes.drain().is_empty(),
        "no probe, no listing at pick time"
    );
    assert!(
        app.browser.is_none(),
        "a Zephyr board must not get a file browser"
    );
}

#[test]
fn the_chip_identity_is_queried_even_on_the_zephyr_backend() {
    // Zephyr runs on ESP32 boards, whose flash runner is esptool --- so a
    // selected port gets the same identification question, answer or not.
    // The reading restarts the board, so it is asked for first (default No);
    // here the answer is yes, and the fake answers, and the Dashboard's
    // Device info pane shows it under a Zephyr project.
    let (mut app, root) = zephyr_app("chip-id");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert!(app.devices.selected_port().is_some());

    // The question opens on the next tick; nothing has touched the port
    // before it.
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    assert!(
        !app.flash.as_ref().is_some_and(|flash| flash.is_busy()),
        "no query may run before the answer"
    );
    app.handle(key(KeyCode::Char('y')));

    // The deferred query runs and lands in the panel.
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.family.is_some()),
        20
    ));
    assert!(
        app.browser.is_none(),
        "identity aside, this backend still never lists files"
    );

    let frame = render(&mut app, 110, 32);
    assert!(
        frame.contains("Device Info"),
        "the pane must exist for a build backend too:\n{frame}"
    );
    assert!(frame.contains("ESP32"), "missing chip identity:\n{frame}");
}

/// Gives the app a resolved Zephyr workspace whose west is the named fake
/// binary --- the invocation every monitor of the Zephyr environment (and
/// now the live version capture) runs through.
fn resolve_workspace_with_west(app: &mut App, root: &std::path::Path, west_bin: &str) {
    let workspace = Workspace {
        dir: root.join("ws"),
        origin: WorkspaceOrigin::UserConfig,
        zephyr_base: root.join("ws/zephyr"),
        venv: Some(root.join("ws/.venv")),
        west: fake(west_bin),
        sdk: None,
    };
    app.workspace = Some(WorkspacePanel::new(Resolution::Single(workspace), ""));
}

/// The plain-`west` flavor every existing test in this file uses.
fn resolve_workspace_with_fake_west(app: &mut App, root: &std::path::Path) {
    resolve_workspace_with_west(app, root, "west");
}

/// Answers the board question the platform monitor needs and creates the
/// configured build directory it reads the runner configuration from.
fn answer_board(app: &mut App, root: &std::path::Path, board: &str) {
    std::fs::create_dir_all(root.join("build")).unwrap();
    app.build.as_mut().unwrap().set_picked(board);
}

#[test]
fn m_runs_the_platforms_own_espressif_monitor_on_the_selected_port() {
    let (mut app, root) = zephyr_app("monitor");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert!(app.devices.selected_port().is_some());
    resolve_workspace_with_fake_west(&mut app, &root);
    answer_board(&mut app, &root, "adafruit_feather_esp32s3/esp32s3/procpu");

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
                .any(|l| l.contains("espressif"))
        },
        10,
    );
    assert!(echoed, "no espressif monitor output arrived");
    let port = root.join("dev/ttyACM0").display().to_string();
    assert!(
        app.device_monitor_output
            .iter()
            .any(|l| l.contains("espressif monitor") && l.contains("-p") && l.contains(&port)),
        "the monitor must run the workspace's own espressif extension with \
         the selected port: {:?}",
        app.device_monitor_output
    );
}

#[test]
fn m_without_a_workspace_refuses_instead_of_guessing() {
    let (mut app, root) = zephyr_app("no-ws");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert!(app.devices.selected_port().is_some());
    answer_board(&mut app, &root, "esp32_devkitc_wrover/esp32/procpu");

    // No resolved workspace: the platform monitor runs through its west,
    // and a bare `west` from `PATH` would be a guess about the environment.
    app.handle(key(KeyCode::Char('m')));

    assert!(
        app.device_monitor_process.is_none(),
        "nothing may spawn without the workspace's west"
    );
    let last = last_log(&app);
    assert!(
        last.contains("monitor unavailable") && last.contains("workspace"),
        "the refusal must name the missing fact: {last}"
    );
}

#[test]
fn m_on_a_non_espressif_board_refuses_not_improvises_a_console() {
    let (mut app, root) = zephyr_app("nrf");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert!(app.devices.selected_port().is_some());
    resolve_workspace_with_fake_west(&mut app, &root);
    answer_board(&mut app, &root, "nrf52840dk/nrf52840");

    // Zephyr ships no monitor for nRF: a generic serial viewer would be a
    // monitor at any cost rather than the environment's own form. The
    // refusal names the platform so the user can open their own terminal.
    app.handle(key(KeyCode::Char('m')));

    assert!(
        app.device_monitor_process.is_none(),
        "nothing may spawn for a platform the environment has no monitor for"
    );
    let last = last_log(&app);
    assert!(
        last.contains("monitor unavailable") && last.contains("nrf52840dk"),
        "the refusal must name the platform: {last}"
    );
}

/// The newest log entry's message.
fn last_log(app: &App) -> String {
    app.logs
        .visible(1000)
        .last()
        .map(|entry| entry.message.clone())
        .unwrap_or_default()
}

#[test]
fn project_setup_does_not_clobber_the_device_picker_it_opens() {
    let root = std::env::temp_dir().join(format!("chiptui-zmon-setup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    std::fs::write(root.join("dev/ttyACM1"), b"").unwrap();

    let mut app = App::new(&root);
    // Answering the prompt records the project in the user config, so the
    // home must be redirected before it is answered.
    app.set_home_dir(root.join("home"));
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
fn hotplug_updates_the_device_status() {
    let (mut app, root) = zephyr_app("hotplug");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert_eq!(app.devices.discovery, DiscoveryState::Ready);
    assert!(app.devices.selected_port().is_some());

    // The selection opens the identification question; answering yes runs
    // the chain whose verdict the unplug below must not race. (The question
    // is the one overlay the chain opens --- nothing else needs the user.)
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        20
    ));
    assert_eq!(app.overlay, None);

    // Unplug: the /dev walk the poll counts changes, and the status
    // follows --- this is the connect/disconnect feedback the monitor-only
    // backend never had before.
    std::fs::remove_file(root.join("dev/ttyACM0")).unwrap();
    assert!(pump_until(
        &mut app,
        |app| app.devices.discovery == DiscoveryState::Failed,
        20
    ));
    assert!(
        app.devices.selected().is_none(),
        "the port is gone; the selection must go with it"
    );
    assert!(
        app.flash
            .as_ref()
            .is_some_and(|flash| flash.details.is_empty()),
        "the departed board's identity must not outlive it"
    );

    // Replug: the same walk sees the port again, the selection returns,
    // and --- a replug being a board ChipTUI has not asked about --- the
    // identification question opens again before the identity refills.
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    assert!(pump_until(
        &mut app,
        |app| app.devices.discovery == DiscoveryState::Ready,
        20
    ));
    assert!(app.devices.selected_port().is_some());
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.family.is_some()),
        20
    ));

    // The replugged board is identified again: the disconnect dropped the
    // per-port answer, so `Firmware:` cannot sit at a stale verdict.
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        20
    ));
    assert_eq!(app.overlay, None);
}

#[test]
fn m_without_a_selected_port_refuses_instead_of_guessing() {
    // The platform monitor attaches through the selected port: portless,
    // the espressif extension probes every candidate port with esptool,
    // resetting each board --- exactly the guess this app never makes. A
    // refusal that names the fact beats a silent reset.
    let (mut app, _root) = zephyr_app("auto");
    assert!(app.devices.selected_port().is_none());

    app.handle(key(KeyCode::Char('m')));

    assert!(
        app.device_monitor_process.is_none(),
        "nothing may spawn without a selected port"
    );
    let last = app
        .logs
        .visible(1000)
        .last()
        .map(|entry| entry.message.clone())
        .unwrap_or_default();
    assert!(
        last.contains("monitor unavailable") && last.contains("no device selected"),
        "the refusal must name the missing fact: {last}"
    );
}

#[test]
fn a_west_flash_reidentifies_the_firmware() {
    let (mut app, root) = zephyr_app("reflash");
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));

    // The selection asks first; the accepted answer's chain identifies the
    // firmware once.
    assert!(pump_until(
        &mut app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| app
            .flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        20
    ));
    let identifications = |app: &App| {
        app.logs
            .visible(usize::MAX)
            .filter(|entry| entry.message.contains("firmware on the device"))
            .count()
    };
    assert_eq!(identifications(&app), 1);

    // Flash from the build panel: it needs a board answer (the session
    // state a picker pick would leave behind), then the last lifecycle
    // row, behind its confirm.
    app.build
        .as_mut()
        .unwrap()
        .set_picked("esp32_devkitc_wrover/esp32/procpu");
    app.focus = Focus::Build;
    for _ in 0..6 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::ConfirmBuild { .. })));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(
        &mut app,
        |app| app
            .build
            .as_ref()
            .and_then(|panel| panel.last.as_ref())
            .is_some_and(|last| last.ok),
        20
    ));

    // The flash changed the device: a new identification read runs on its
    // own --- no listing, no keypress, no re-selection to drive it.
    assert!(
        pump_until(&mut app, |app| identifications(app) >= 2, 20),
        "the firmware must be re-read after west flash"
    );
    assert!(
        app.flash
            .as_ref()
            .is_some_and(|flash| flash.details.firmware.is_some()),
        "the re-read must leave a standing verdict"
    );
}

/// Selects the device and drives the identification chain up to a
/// versionless Zephyr verdict --- the point every test below starts from.
fn select_and_identify(app: &mut App, root: &std::path::Path) {
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert!(pump_until(
        app,
        |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        20
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(
        pump_until(
            app,
            |app| matches!(
                app.flash.as_ref().unwrap().details.firmware,
                Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None))
            ),
            20
        ),
        "the identification read must name Zephyr without a version first"
    );
}

#[test]
fn live_boot_banner_capture_dates_a_versionless_zephyr_verdict() {
    // esp32c3-round-display's exact shape: a Zephyr simple-boot image whose
    // banner sits past both flash-byte windows, but whose live boot banner
    // (after esptool's own post-read reset) still names it.
    let (mut app, root) = zephyr_app("live-hit");
    app.flash
        .as_mut()
        .unwrap()
        .set_tool_path(fake("esptool-zephyr-no-version"));
    // Both prerequisites must be in place *before* the identification read
    // lands: the version hunt is tried in the same event that arms it, so
    // answering the board afterward would be too late.
    resolve_workspace_with_west(&mut app, &root, "west-zephyr-banner");
    answer_board(&mut app, &root, "adafruit_feather_esp32s3/esp32s3/procpu");
    select_and_identify(&mut app, &root);
    let before = (app.focus, app.log_tab, app.monitor_source);

    assert!(
        pump_until(
            &mut app,
            |app| matches!(
                app.flash.as_ref().unwrap().details.firmware,
                Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, Some(_)))
            ),
            20
        ),
        "the live boot-banner capture never dated the verdict: {:?}",
        app.flash.as_ref().unwrap().details.firmware
    );
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(
            FlashFirmware::Zephyr,
            Some("v4.4.0-11847-gc5dffcb7c9da".to_string())
        ))
    );
    assert!(
        app.logs
            .visible(usize::MAX)
            .any(|entry| entry.message.contains("Zephyr build v4.4.0")),
        "the live capture must log the same notice the byte hunt would"
    );

    // Invisible background courtesy work: no UI hijack, same rule the
    // chip-id/firmware-read queries already follow.
    assert!(
        app.device_monitor_process.is_none(),
        "the capture must never become the interactive Monitor tab's session"
    );
    assert_eq!(
        (app.focus, app.log_tab, app.monitor_source),
        before,
        "the capture must not move focus, switch tabs or claim the monitor source"
    );
}

#[test]
fn the_byte_hunt_still_runs_without_a_resolved_workspace() {
    // No `resolve_workspace_with_west`/`answer_board` at all: the live
    // capture's own prerequisite check (the same one `open_monitor` uses)
    // must refuse it, and the flash-byte hunt --- which this fixture's hunt
    // window also leaves blank on purpose --- is what settles the verdict.
    let (mut app, root) = zephyr_app("live-no-workspace");
    app.flash
        .as_mut()
        .unwrap()
        .set_tool_path(fake("esptool-zephyr-no-version"));
    select_and_identify(&mut app, &root);

    assert!(
        pump_until(
            &mut app,
            |app| !app.flash.as_ref().unwrap().has_pending_version_hunt(),
            20
        ),
        "the byte hunt must still settle the verdict without a workspace"
    );
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None)),
        "this fixture's hunt window is blank on purpose: no version to find, live or byte"
    );
}

#[test]
fn the_byte_hunt_runs_after_a_live_capture_finds_nothing() {
    // The platform monitor attaches (idf_monitor's own startup banner) but
    // the board never prints anything recognizable: the live attempt must
    // still fall through to the byte hunt rather than leaving the pending
    // flag stuck forever.
    let (mut app, root) = zephyr_app("live-miss");
    app.flash
        .as_mut()
        .unwrap()
        .set_tool_path(fake("esptool-zephyr-no-version"));
    resolve_workspace_with_west(&mut app, &root, "west-zephyr-silent");
    answer_board(&mut app, &root, "adafruit_feather_esp32s3/esp32s3/procpu");
    select_and_identify(&mut app, &root);

    assert!(
        pump_until(
            &mut app,
            |app| !app.flash.as_ref().unwrap().has_pending_version_hunt(),
            20
        ),
        "the hunt must settle (live miss, then the byte hunt) instead of hanging pending forever"
    );
    assert_eq!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None)),
        "neither the live capture nor the byte hunt found a version here, on purpose"
    );
    assert!(
        app.device_monitor_process.is_none(),
        "the failed live attempt must not leave a monitor session dangling"
    );
}
