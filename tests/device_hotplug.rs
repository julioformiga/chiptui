//! The 1s hotplug poll (`App::check_device_hotplug`) racing the
//! identification chain (`App::confirm_identify_device` →
//! `FlashPanel::query_firmware_identity`/`query_firmware_version`).
//!
//! `esptool` resets the board to read its chip and firmware, and on a
//! native-USB board (ESP32-S3/C3, …) that reset makes the tty node vanish
//! from `/dev` for the reset window. Without a guard on the flash panel's
//! own busy state, the poll reads that blip as a real disconnect and calls
//! `App::device_disconnected`, which wipes the firmware verdict the chain
//! is mid-flight on — the visible symptom was the Device info pane settling
//! on a bare "Zephyr" and never gaining its version, because the follow-up
//! `read-flash` that dates it either got dropped mid-run or landed after
//! `clear_device_details` had already reset the verdict it was going to
//! fill in.

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::app::{App, Overlay};
use chiptui::backend::BackendKind;
use chiptui::event::AppEvent;
use chiptui::firmware_id::{FirmwareVerdict, FlashFirmware};
use chiptui::flash::FlashPanel;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A fake `esptool` that answers `chip-id` and the two identification reads
/// (bootloader window, then the version-hunt window) with a real Zephyr
/// image's shape: the app's own banner sits well past the first window,
/// which is exactly the layout `firmware_id::HUNT_OFFSET` exists for.
fn write_fake_esptool(path: &std::path::Path) {
    std::fs::write(
        path,
        concat!(
            "#!/bin/sh\n",
            "args=\"$*\"\n",
            "dest=$(printf '%s\\n' \"$@\" | tail -n 1)\n",
            "case \"$args\" in\n",
            "    *\"chip-id\"*)\n",
            "        sleep 2\n",
            "        printf 'esptool v5.3.1\\n'\n",
            "        printf 'Detecting chip type... ESP32\\n'\n",
            "        printf 'Chip is ESP32-D0WD (revision 3)\\n'\n",
            "        printf 'Features: WiFi, BT, Dual Core, 240MHz\\n'\n",
            "        printf 'Crystal is 40MHz\\n'\n",
            "        printf 'MAC: 24:6f:28:12:34:56\\n'\n",
            "        ;;\n",
            "    *\"read-flash 0x0 0x20000\"*)\n",
            "        head -c 131072 /dev/zero | tr '\\000' '\\377' > \"$dest\"\n",
            "        printf '>>> ZEPHYR FATAL ERROR %%d: %%s on CPU %%d\\n' \\\n",
            "            | dd of=\"$dest\" bs=1 seek=2560 conv=notrunc status=none\n",
            "        printf 'Read 131072 bytes\\n'\n",
            "        ;;\n",
            "    *\"read-flash 0x20000 0x100000\"*)\n",
            "        head -c 1048576 /dev/zero | tr '\\000' '\\377' > \"$dest\"\n",
            "        printf '*** Booting Zephyr OS build v4.4.0-11847-gc5dffcb7c9da ***\\n' \\\n",
            "            | dd of=\"$dest\" bs=1 seek=100000 conv=notrunc status=none\n",
            "        printf 'Read 1048576 bytes\\n'\n",
            "        ;;\n",
            "    *)\n",
            "        echo 'A fatal error occurred: Could not open port' >&2\n",
            "        exit 1\n",
            "        ;;\n",
            "esac\n",
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

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

fn zephyr_hunted(app: &App) -> bool {
    matches!(
        app.flash.as_ref().unwrap().details.firmware,
        Some(FirmwareVerdict::Firmware(FlashFirmware::Zephyr, Some(_)))
    )
}

/// Aligns `app.ticks` on a hotplug-poll boundary (`check_device_hotplug`
/// only samples every 4th tick), so the caller's own filesystem change
/// lands squarely inside one poll window instead of at the mercy of
/// whatever tick count the chain above happened to leave behind.
fn align_to_poll_boundary(app: &mut App) {
    while !app.ticks.is_multiple_of(4) {
        app.handle(AppEvent::Tick);
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn version_hunt_runs_after_granted_identification() {
    let root = std::env::temp_dir().join(format!("chiptui-hunt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    write_fake_esptool(&root.join("esptool"));

    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    let mut flash = FlashPanel::new(&root);
    flash.set_tool_path(root.join("esptool").display().to_string());
    app.flash = Some(flash);
    app.maybe_scan_devices();

    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert!(app.devices.selected_port().is_some());

    assert!(
        pump_until(
            &mut app,
            |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
            20
        ),
        "identification question never opened"
    );
    app.handle(key(KeyCode::Char('y')));

    assert!(
        pump_until(&mut app, zephyr_hunted, 20),
        "the version hunt never filled the Zephyr version: {:?}",
        app.flash.as_ref().unwrap().details.firmware
    );
}

#[test]
fn version_hunt_runs_in_the_micropython_flow() {
    let root = std::env::temp_dir().join(format!("chiptui-hunt-mpy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    write_fake_esptool(&root.join("esptool"));

    let mut app = App::new(std::env::temp_dir());
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::MicroPython));
    let mut browser = chiptui::browser::Browser::new(std::env::temp_dir());
    browser.set_tool_path(format!(
        "{}/tests/fixtures/bin/mpremote",
        env!("CARGO_MANIFEST_DIR")
    ));
    app.browser = Some(browser);
    let mut flash = FlashPanel::new(std::env::temp_dir());
    flash.set_tool_path(root.join("esptool").display().to_string());
    app.flash = Some(flash);
    app.maybe_scan_devices();
    app.handle(key(KeyCode::Char('d')));

    assert!(
        pump_until(
            &mut app,
            |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
            20
        ),
        "identification question never opened"
    );
    app.handle(key(KeyCode::Char('y')));

    assert!(
        pump_until(&mut app, zephyr_hunted, 20),
        "the version hunt never filled the Zephyr version: {:?}",
        app.flash.as_ref().unwrap().details.firmware
    );
}

/// Real-hardware shape: esptool resets the board into its bootloader to
/// read the chip; on native-USB boards the port vanishes from `/dev` for
/// the reset window. The hotplug poll must not treat that as a disconnect
/// while the chain the user authorized is running (`App::check_device_hotplug`'s
/// `FlashPanel::is_busy` guard) — otherwise it drops the identification
/// mid-flight and the pane is left on a bare "Zephyr" with no version.
#[test]
fn a_port_blip_during_the_chip_query_does_not_kill_the_chain() {
    let root = std::env::temp_dir().join(format!("chiptui-hunt-blip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    write_fake_esptool(&root.join("esptool"));

    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    let mut flash = FlashPanel::new(&root);
    flash.set_tool_path(root.join("esptool").display().to_string());
    app.flash = Some(flash);
    app.maybe_scan_devices();

    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.handle(key(KeyCode::Char('d')));
    assert!(app.devices.selected_port().is_some());

    assert!(
        pump_until(
            &mut app,
            |app| matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
            20
        ),
        "identification question never opened"
    );
    app.handle(key(KeyCode::Char('y')));
    assert!(
        app.flash.as_ref().is_some_and(|f| f.is_busy()),
        "the chip query should be running after the yes"
    );

    // Seed the hotplug poll's baseline port count while the port is
    // present (it only rescans on a *change*), then blip it: gone, then
    // back, the way a native-USB board's reset looks from `/dev`.
    align_to_poll_boundary(&mut app);
    app.handle(AppEvent::Tick);
    std::fs::remove_file(root.join("dev/ttyACM0")).unwrap();
    align_to_poll_boundary(&mut app);
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();

    assert!(
        pump_until(&mut app, zephyr_hunted, 20),
        "the port blip during the chip query killed the authorized chain: {:?}",
        app.flash.as_ref().unwrap().details.firmware
    );
    assert!(
        !matches!(app.overlay, Some(Overlay::ConfirmIdentifyDevice { .. })),
        "the blip must not have re-asked for identification"
    );
}
