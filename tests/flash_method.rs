//! `Flash`'s one question, through the `App`: which way the firmware gets
//! there.
//!
//! The three rules the rows follow live in
//! `backend::zephyr::flash_method`'s own unit tests, where they are pure.
//! What is asserted here is the wiring: that the row asks at all, that the
//! answer reaches the §15 confirm and the OTA modal respectively, that a
//! dimmed row refuses from one place whether it is pressed or clicked, and
//! that the drawn geometry and the walked geometry are the same two rows.
//!
//! The no-capability branch (a build-panel backend that cannot update over
//! the air goes straight to the confirm) has no fixture: Zephyr is the only
//! backend with a build panel, and MicroPython's `x` --- covered in
//! `tests/build_view.rs` --- lands on its own Actions tab instead.

#![cfg(unix)]

use chiptui::app::{App, Focus, Overlay};
use chiptui::backend::BackendKind;
use chiptui::build::BuildAction;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{click, fake, key, render};

/// A buildable Zephyr project with a board answer, its own `/dev` and its
/// own `$HOME`, so neither the serial scan nor workspace discovery can see
/// the developer's machine.
fn zephyr_app(tag: &str, ota: Option<&str>) -> (App, std::path::PathBuf) {
    let root =
        std::env::temp_dir().join(format!("chiptui-flashmethod-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("build/zephyr")).unwrap();
    std::fs::write(
        root.join("build/zephyr/CMakeCache.txt"),
        "CACHED_BOARD:STRING=nrf52840dk/nrf52840\n",
    )
    .unwrap();
    if let Some(transport) = ota {
        std::fs::write(
            root.join("chiptui.toml"),
            format!("[ota]\ntransport = \"{transport}\"\n"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(root.join("home"));
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.maybe_scan_devices();
    app.place_startup_focus();
    app.build.as_mut().unwrap().set_tool_path(fake("west"));
    (app, root)
}

/// Plants a USB serial port and rescans. The identification question is
/// only *armed* by this --- its overlay opens on a tick, and no test here
/// pumps one before answering the menu.
fn plug_board(app: &mut App, root: &std::path::Path) {
    std::fs::write(root.join("dev/ttyACM0"), b"").unwrap();
    app.scan_serial_devices();
}

/// Presses the Actions pane's `Flash` row, the way a user reaches the menu.
fn press_flash(app: &mut App) {
    app.focus = Focus::Build;
    let caps = app.manager.capabilities();
    let panel = app.build.as_mut().expect("a build panel");
    panel.cursor = panel
        .actions(&caps)
        .iter()
        .position(|action| *action == BuildAction::Flash)
        .expect("Flash is in the action list");
    app.handle(key(KeyCode::Enter));
}

/// The drawn row and column of `needle`'s first cell.
fn find_cell(frame: &str, needle: &str) -> Option<(u16, u16)> {
    frame.lines().enumerate().find_map(|(row, line)| {
        line.find(needle)
            .map(|byte| (row as u16, line[..byte].chars().count() as u16))
    })
}

#[test]
fn flash_asks_which_way_before_it_writes_anything() {
    let (mut app, root) = zephyr_app("asks", None);
    plug_board(&mut app, &root);

    press_flash(&mut app);
    assert!(
        matches!(app.overlay, Some(Overlay::FlashMethod { .. })),
        "the row asks first: {:?}",
        app.overlay
    );
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Flash over USB") && frame.contains("OTA update (udp)"),
        "both ways are on the menu:\n{frame}"
    );
    assert!(
        frame.contains("west flash") && frame.contains("/dev/ttyACM0"),
        "the wired row names what it would run, and where:\n{frame}"
    );

    // Nothing has started: this menu chooses a path, it does not write.
    assert!(!app.build.as_ref().unwrap().is_busy());

    // The wired row leads to the §15 confirm, unchanged --- the literal
    // command, defaulting to No.
    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmBuild {
            action: BuildAction::Flash,
            confirm: false
        })
    );
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("west flash"),
        "the confirm still quotes the command:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The menu opens even when only one row can run --- that is the whole
/// reason it opens: a dimmed row with its reason under it is the only place
/// the user reads why the other way is unavailable.
#[test]
fn an_empty_usb_bus_dims_the_wired_row_and_says_why() {
    let (mut app, root) = zephyr_app("no-device", None);

    press_flash(&mut app);
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("Flash over USB") && frame.contains("no device connected"),
        "the dimmed row carries its own reason:\n{frame}"
    );

    // The cursor opens on the row that can run, so the reflex `Enter` is
    // never a no-op.
    assert!(matches!(
        app.overlay,
        Some(Overlay::FlashMethod { selected: 1, .. })
    ));
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "the OTA row opens the modal: {:?}",
        app.overlay
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The one row whose availability reads the project's configuration: a
/// serial transport travels the same cable the wired row does.
#[test]
fn a_serial_transport_dims_the_ota_row_too() {
    let (mut app, root) = zephyr_app("serial", Some("serial"));

    press_flash(&mut app);
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("OTA update (serial)") && frame.contains("the OTA transport is serial"),
        "the OTA row explains its own refusal:\n{frame}"
    );

    // Nothing can run, so the cursor stays on the first row --- and `Enter`
    // there does nothing at all.
    assert!(matches!(
        app.overlay,
        Some(Overlay::FlashMethod { selected: 0, .. })
    ));
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::FlashMethod { .. })),
        "a dimmed row is a no-op, not a confirm: {:?}",
        app.overlay
    );
    assert!(!app.build.as_ref().unwrap().is_busy());

    // Plug the board in and the same project offers both ways.
    app.handle(key(KeyCode::Esc));
    plug_board(&mut app, &root);
    press_flash(&mut app);
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "with the cable there, the serial transport is a real answer: {:?}",
        app.overlay
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The drawn count and the walked count are one number (`flash_method::
/// COUNT`), the discipline `ZEPHYR_ACTIONS_COUNT` keeps.
#[test]
fn the_cursor_walks_exactly_the_rows_that_are_drawn() {
    let (mut app, root) = zephyr_app("walk", None);
    plug_board(&mut app, &root);

    press_flash(&mut app);
    app.handle(key(KeyCode::Down));
    assert!(matches!(
        app.overlay,
        Some(Overlay::FlashMethod { selected: 1, .. })
    ));
    app.handle(key(KeyCode::Down));
    assert!(
        matches!(app.overlay, Some(Overlay::FlashMethod { selected: 0, .. })),
        "two rows, wrapping"
    );
    app.handle(key(KeyCode::Up));
    assert!(matches!(
        app.overlay,
        Some(Overlay::FlashMethod { selected: 1, .. })
    ));

    // `q` closes, like every other stacked menu: no letter means anything
    // here, so it is free to.
    app.handle(key(KeyCode::Char('q')));
    assert_eq!(app.overlay, None);
    let _ = std::fs::remove_dir_all(&root);
}

/// The click grammar, including the bug family this menu inherits: a
/// stacked menu's row lookup keys on the row alone, so a click past either
/// edge of the box must be read as "outside" *before* any row is found.
#[test]
fn a_click_presses_a_row_and_one_outside_the_box_closes_it() {
    let (mut app, root) = zephyr_app("click", None);
    plug_board(&mut app, &root);
    app.set_mouse_enabled(true);

    press_flash(&mut app);
    let frame = render(&mut app, 100, 32);
    let (row, column) = find_cell(&frame, "OTA update").expect("the OTA row is drawn");

    // Same row, but past the popup's right edge: the click closes the menu
    // instead of quietly answering the row it lines up with.
    app.handle(chiptui::event::AppEvent::Mouse(click(99, row)));
    assert_eq!(
        app.overlay, None,
        "a click beside the box closes it, like Esc"
    );

    // On the row itself, it presses --- the same `Enter` the keyboard sends.
    press_flash(&mut app);
    app.handle(chiptui::event::AppEvent::Mouse(click(column, row)));
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "the click opens what the row leads to: {:?}",
        app.overlay
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A dimmed row refuses a click for the same reason it refuses `Enter`, and
/// in the same place: the click presses through `Enter`, so there is one
/// gate, not two.
#[test]
fn a_click_on_a_dimmed_row_does_nothing() {
    let (mut app, root) = zephyr_app("click-dim", None);
    app.set_mouse_enabled(true);

    press_flash(&mut app);
    let frame = render(&mut app, 100, 32);
    let (row, column) = find_cell(&frame, "Flash over USB").expect("the wired row is drawn");
    app.handle(chiptui::event::AppEvent::Mouse(click(column, row)));
    assert!(
        matches!(app.overlay, Some(Overlay::FlashMethod { selected: 0, .. })),
        "the click selects the row it landed on and stops there: {:?}",
        app.overlay
    );
    assert!(!app.build.as_ref().unwrap().is_busy());
    let _ = std::fs::remove_dir_all(&root);
}
