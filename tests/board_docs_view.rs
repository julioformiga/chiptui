//! The board/shield pickers' docs enrichment, end to end over `App`: the
//! west lists stay the picker's spine, the row under the cursor gains the
//! Zephyr docs index's picture and documentation text, fetched through the
//! injectable transport (no network in tests), and the details pane pages
//! with pgup/pgdn like every scrolling pane here.

#![cfg(unix)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use chiptui::app::{App, DocsFocus, Overlay};
use chiptui::backend::BackendKind;
use chiptui::backend::zephyr::workspace::{Workspace, WorkspaceOrigin};
use chiptui::board_docs::{DocsEvent, IndexState};
use chiptui::event::AppEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn fake(tool: &str) -> String {
    format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> KeyCode {
    KeyCode::Char(c)
}

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, modifiers))
}

/// The Project pane's way in: the shortcuts overlay (`ctrl+k`), then the
/// pane's `e` letter.
fn enter_project_pane(app: &mut App) {
    app.handle(key_event(ctrl('k'), KeyModifiers::CONTROL));
    app.handle(key(KeyCode::Char('e')));
}

/// A Zephyr app in a temp directory whose build panel runs the fake west
/// (see `tests/build_view.rs`; same shape, minus what those tests already
/// cover).
fn picker_app(tag: &str) -> App {
    let root = std::env::temp_dir().join(format!("chiptui-docsview-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
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
    app
}

/// Opens the board picker through the Project pane's Board · Shield row.
fn open_board_picker(app: &mut App) {
    enter_project_pane(app);
    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Enter));
}

/// Opens the shield picker: the same row, shield segment.
fn open_shield_picker(app: &mut App) {
    enter_project_pane(app);
    for _ in 0..3 {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Right));
    app.handle(key(KeyCode::Enter));
}

/// Filters the picker down to the nRF rows, so row 0 is
/// `nrf52840dk/nrf52840` --- the one board the fixture docs site fully
/// serves (picture, page text and all).
fn focus_nrf(app: &mut App) {
    for c in ['n', 'r', 'f'] {
        app.handle(key(KeyCode::Char(c)));
    }
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| chiptui::ui::draw(frame, app))
        .unwrap();
    terminal.backend().to_string()
}

/// Drains process and docs events plus ticks until `done` holds --- the
/// binary loop's own cadence, with the docs channel beside the processes.
fn pump_until(app: &mut App, mut done: impl FnMut(&App) -> bool, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        for event in app.docs.drain() {
            app.handle(AppEvent::Docs(event));
        }
        if done(app) {
            return true;
        }
        app.handle(AppEvent::Tick);
        std::thread::sleep(Duration::from_millis(5));
    }
    done(app)
}

/// The boards index, in the real page's card markup, covering the join
/// cases the fake west's list produces: a board with a picture
/// (`nrf52840dk`), one without (`esp32_devkitc_wrover`), and a shield
/// (`link_board_eth`).
const INDEX_HTML: &str = r#"
<html><body><div id="catalog">
<a class="board-card" href="../boards/nordic/nrf52840dk/doc/index.html">
  <div class="vendor">Nordic Semiconductor ASA</div>
  <img src="../_images/nrf52840dk.jpg" class="picture" />
  <div class="board-name">nRF52840 DK</div>
  <div class="arch">arm</div>
</a>
<a class="board-card" href="../boards/espressif/esp32_devkitc_wrover/doc/index.html">
  <div class="vendor">Espressif</div>
  <div class="board-name">ESP32 DevKitC-WROVER</div>
  <div class="arch">xtensa</div>
</a>
<a class="board-card shield" href="../boards/shields/link_board_eth/doc/index.html">
  <div class="vendor">WIZnet</div>
  <div class="board-name">W5500 Ethernet Shield</div>
</a>
</div></body></html>
"#;

const NRF_DETAIL_HTML: &str = r#"
<html><body><div itemprop="articleBody">
<h1>nRF52840 DK</h1>
<p>The nRF52840 DK is a single-board development kit for Bluetooth 5, NFC, Thread and Zigbee.</p>
</div></body></html>
"#;

/// A minimal 1x1 PNG (green pixel): enough for `image::load_from_memory`
/// to decode a real picture out of the fetch path.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0,
    0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// The offline docs site: index, the nRF board's page, and its picture.
fn docs_fetch() -> chiptui::board_docs::Fetch {
    Arc::new(|url: &str| {
        if url.ends_with("/boards/index.html") {
            Ok(INDEX_HTML.as_bytes().to_vec())
        } else if url.ends_with("nrf52840dk/doc/index.html") {
            Ok(NRF_DETAIL_HTML.as_bytes().to_vec())
        } else if url.ends_with("nrf52840dk.jpg") {
            Ok(TINY_PNG.to_vec())
        } else {
            Err(std::io::Error::other("not found"))
        }
    })
}

#[test]
fn the_board_picker_enriches_the_row_under_the_cursor() {
    let mut app = picker_app("enrich");
    app.docs.set_fetch(docs_fetch());
    open_board_picker(&mut app);
    assert!(matches!(app.overlay, Some(Overlay::BoardPicker { .. })));
    focus_nrf(&mut app);

    // The west list is the spine; the docs index enriches it: both the
    // page text and the decoded picture must land.
    let settled = pump_until(
        &mut app,
        |app| app.docs.details.contains_key("nrf52840dk") && app.docs.has_image("nrf52840dk"),
        10,
    );
    assert!(settled, "the docs fetches never finished");
    assert!(
        app.docs.protocol_for("nrf52840dk").is_some(),
        "a fetched picture must become a renderable protocol"
    );

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("nrf52840dk/nrf52840"),
        "the west list:\n{frame}"
    );
    assert!(frame.contains("nRF52840 DK"), "the docs name:\n{frame}");
    assert!(
        frame.contains("Nordic Semiconductor ASA"),
        "the vendor:\n{frame}"
    );
    // The pane wraps at its own width, so the assertions are
    // wrap-agnostic: each fragment fits the column even at the minimum.
    assert!(frame.contains("Bluetooth 5,"), "the detail text:\n{frame}");
    assert!(
        frame.contains("NFC,"),
        "the detail text continues:\n{frame}"
    );
    assert!(
        frame.contains("Thread and Zigbee."),
        "the detail tail:\n{frame}"
    );
    assert!(
        frame.contains("(west boards)"),
        "the source stays named:\n{frame}"
    );
}

#[test]
fn a_board_without_a_docs_entry_says_so() {
    let mut app = picker_app("no-entry");
    app.docs.set_fetch(docs_fetch());
    open_board_picker(&mut app);
    for c in ['r', 'p', 'i'] {
        app.handle(key(KeyCode::Char(c)));
    }
    // Both lists: the west spine and the docs enrichment.
    let loaded = pump_until(
        &mut app,
        |app| {
            *app.docs.state() == IndexState::Loaded
                && matches!(
                    app.build.as_ref().unwrap().boards.state,
                    chiptui::build::ListState::Loaded(_)
                )
        },
        10,
    );
    assert!(loaded, "the lists never loaded");

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("not in the Zephyr docs index"),
        "an unmatched target is a named state:\n{frame}"
    );
    // The west description still tells the user what the board is
    // (truncated at the list's fixed column, like every list row).
    assert!(frame.contains("Raspberry Pi"), "the west row:\n{frame}");
}

#[test]
fn the_shield_picker_enriches_and_none_has_nothing_to_look_up() {
    let mut app = picker_app("shield-enrich");
    app.docs.set_fetch(docs_fetch());
    open_shield_picker(&mut app);
    assert!(matches!(app.overlay, Some(Overlay::ShieldPicker { .. })));

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("(none)"),
        "the clear row must stay:\n{frame}"
    );
    assert!(
        frame.contains("the shield is"),
        "the (none) row explains itself (wrapped at the pane's width):\n{frame}"
    );

    // The west list has to be here before the cursor can step past the
    // (none) row (an unloaded list clamps every step to row 0).
    let loaded = pump_until(
        &mut app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().shields.state,
                chiptui::build::ListState::Loaded(_)
            )
        },
        10,
    );
    assert!(loaded, "the fake west shields never finished");
    // One Down: past the (none) row onto link_board_eth, whose docs entry
    // names the vendor the pane should show.
    app.handle(key(KeyCode::Down));
    let settled = pump_until(&mut app, |app| app.docs.entry_settled("link_board_eth"), 10);
    assert!(
        settled,
        "the shield's docs never arrived: state={:?} overlay={:?}",
        app.docs.state(),
        app.overlay
    );
    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("W5500 Ethernet Shield"),
        "the docs name:\n{frame}"
    );
    assert!(frame.contains("WIZnet"), "the vendor:\n{frame}");
}

#[test]
fn the_details_pane_pages_and_a_new_row_resets_it() {
    let mut app = picker_app("scroll");
    app.docs.set_fetch(docs_fetch());
    open_board_picker(&mut app);
    let loaded = pump_until(
        &mut app,
        |app| {
            *app.docs.state() == IndexState::Loaded
                && matches!(
                    app.build.as_ref().unwrap().boards.state,
                    chiptui::build::ListState::Loaded(_)
                )
        },
        10,
    );
    assert!(loaded);
    focus_nrf(&mut app);

    // A long documentation page arrives through the public event path (the
    // transport is covered above; here the pane's scrolling is the subject).
    let detail = (0..100)
        .map(|line| format!("pin mux line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.handle(AppEvent::Docs(DocsEvent::Entry {
        id: "nrf52840dk".to_string(),
        detail: Some(detail),
        image: None,
    }));
    let frame = render(&mut app, 100, 32);
    assert!(frame.contains("pin mux line 0"), "the page top:\n{frame}");
    assert!(
        app.docs_viewport > 1,
        "the renderer published the pane height"
    );

    app.handle(key(KeyCode::PageDown));
    assert!(
        matches!(app.overlay, Some(Overlay::BoardPicker { scroll, .. }) if scroll > 0),
        "pgdn must move the details"
    );
    let frame = render(&mut app, 100, 32);
    assert!(
        !frame.contains("pin mux line 0"),
        "the pane scrolled past the top:\n{frame}"
    );

    // A filter change restarts the (new) row's details from the top.
    app.handle(key(KeyCode::Char('z')));
    assert!(
        matches!(app.overlay, Some(Overlay::BoardPicker { scroll, .. }) if scroll == 0),
        "a new row starts at the top"
    );
}

/// `Tab` hands the details pane the keyboard: the arrows scroll the docs
/// text instead of walking the list, while printable keys keep filtering
/// and `Enter` keeps answering the row under the list cursor --- the same
/// single keyboard, pointed at the pane the user is reading.
#[test]
fn tab_hands_the_details_the_keyboard_and_back() {
    let mut app = picker_app("focus");
    app.docs.set_fetch(docs_fetch());
    open_board_picker(&mut app);
    let loaded = pump_until(
        &mut app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().boards.state,
                chiptui::build::ListState::Loaded(_)
            )
        },
        10,
    );
    assert!(loaded);
    focus_nrf(&mut app);

    // A page longer than the pane, so line scrolling has somewhere to go.
    let detail = (0..100)
        .map(|line| format!("pin mux line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.handle(AppEvent::Docs(DocsEvent::Entry {
        id: "nrf52840dk".to_string(),
        detail: Some(detail),
        image: None,
    }));
    let frame = render(&mut app, 100, 32);
    assert!(frame.contains("pin mux line 0"), "the page top:\n{frame}");

    // Over to the details: Down scrolls one line, and the list cursor
    // stays exactly where it was.
    app.handle(key(KeyCode::Tab));
    assert!(
        matches!(
            &app.overlay,
            Some(Overlay::BoardPicker {
                focus: DocsFocus::Details,
                selected: 0,
                ..
            })
        ),
        "tab hands the details the keyboard"
    );
    app.handle(key(KeyCode::Down));
    assert!(
        matches!(
            &app.overlay,
            Some(Overlay::BoardPicker {
                focus: DocsFocus::Details,
                selected: 0,
                scroll: 1,
                ..
            })
        ),
        "the arrow scrolls the details, not the list"
    );
    let frame = render(&mut app, 100, 32);
    assert!(
        !frame.contains("Vendor Nordic"),
        "the pane scrolled past its first row:\n{frame}"
    );
    assert!(
        frame.contains("pin mux line 1"),
        "and one line is all it moved:\n{frame}"
    );

    // Typing is still filtering, keyboard or no keyboard: the details
    // pane never eats the filter, and a changed row restarts at the top.
    app.handle(key(KeyCode::Char('z')));
    assert!(
        matches!(
            &app.overlay,
            Some(Overlay::BoardPicker {
                input, scroll: 0, focus: DocsFocus::Details, ..
            }) if input == "nrfz"
        ),
        "printable keys keep filtering while the details hold the keyboard"
    );
    app.handle(key(KeyCode::Backspace));

    // Back to the list: the arrows walk it again, the details restart.
    app.handle(key(KeyCode::Tab));
    app.handle(key(KeyCode::Down));
    assert!(
        matches!(
            &app.overlay,
            Some(Overlay::BoardPicker {
                focus: DocsFocus::List,
                selected: 1,
                scroll: 0,
                ..
            })
        ),
        "the arrows walk the list once it holds the keyboard again"
    );

    // Enter answers the list even while the details hold the keyboard.
    app.handle(key(KeyCode::Tab));
    app.handle(key(KeyCode::Enter));
    assert!(app.overlay.is_none(), "enter applies and closes");
    assert_eq!(
        app.build.as_ref().unwrap().board_name(),
        Some("nrf52840dk/nrf52840/qspi"),
        "the row under the list cursor is what applied"
    );
}

/// Both panes carry a scrollbar only when their content overflows the
/// pane (`┃` is the shared bar's thumb, drawn by nothing else).
#[test]
fn the_panes_show_a_scrollbar_only_when_they_overflow() {
    let mut app = picker_app("scrollbar");
    app.docs.set_fetch(docs_fetch());
    open_board_picker(&mut app);
    let loaded = pump_until(
        &mut app,
        |app| {
            *app.docs.state() == IndexState::Loaded
                && matches!(
                    app.build.as_ref().unwrap().boards.state,
                    chiptui::build::ListState::Loaded(_)
                )
        },
        10,
    );
    assert!(loaded);

    // One board, a short details pane: nothing overflows, so no thumb
    // anywhere (`rpi` leaves exactly the Pico row, whose target the docs
    // index does not know --- a one-line named state).
    for c in ['r', 'p', 'i'] {
        app.handle(key(KeyCode::Char(c)));
    }
    let frame = render(&mut app, 100, 32);
    assert!(!frame.contains('┃'), "nothing overflows:\n{frame}");

    // A docs page longer than the pane: the details scrollbar appears.
    for _ in 0..3 {
        app.handle(key(KeyCode::Backspace));
    }
    focus_nrf(&mut app);
    let detail = (0..100)
        .map(|line| format!("pin mux line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.handle(AppEvent::Docs(DocsEvent::Entry {
        id: "nrf52840dk".to_string(),
        detail: Some(detail),
        image: None,
    }));
    let frame = render(&mut app, 100, 32);
    assert!(frame.contains('┃'), "the details overflow:\n{frame}");

    // And the list itself: with the filter cleared the sixteen rows no
    // longer fit the pane (the board back under the cursor has no docs
    // entry, so the short details cannot be the source).
    for _ in 0..3 {
        app.handle(key(KeyCode::Backspace));
    }
    let frame = render(&mut app, 100, 32);
    assert!(frame.contains('┃'), "the list overflows:\n{frame}");
    assert!(
        frame.contains("native/native64"),
        "the unfiltered list's rows:\n{frame}"
    );
}

#[test]
fn the_docs_label_follows_the_resolved_workspace() {
    let mut app = picker_app("label");
    // A resolved workspace whose checkout carries a VERSION: the docs must
    // come from that release, not `latest`.
    let root = std::env::temp_dir().join(format!("chiptui-docsview-ws-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("zephyr")).unwrap();
    std::fs::write(
        root.join("zephyr/VERSION"),
        "VERSION_MAJOR = 4\nVERSION_MINOR = 2\nPATCHLEVEL = 0\n",
    )
    .unwrap();
    app.workspace
        .as_mut()
        .expect("a Zephyr app has the panel")
        .resolved = Some(Workspace {
        dir: root.clone(),
        origin: WorkspaceOrigin::UserConfig,
        zephyr_base: root.join("zephyr"),
        venv: None,
        west: fake("west"),
        sdk: None,
    });

    assert_eq!(app.docs_label(), "4.2");
    app.docs
        .set_fetch(Arc::new(|_: &str| Ok(INDEX_HTML.as_bytes().to_vec())));
    open_board_picker(&mut app);
    let loaded = pump_until(&mut app, |app| *app.docs.state() == IndexState::Loaded, 10);
    assert!(loaded);
    assert_eq!(app.docs.label(), "4.2", "the release label in use");
}

#[test]
fn the_picker_fits_the_declared_minimum() {
    let mut app = picker_app("minimum");
    app.docs.set_fetch(docs_fetch());
    open_board_picker(&mut app);
    focus_nrf(&mut app);
    let settled = pump_until(
        &mut app,
        |app| app.docs.details.contains_key("nrf52840dk") && app.docs.has_image("nrf52840dk"),
        10,
    );
    assert!(settled);

    // 80x32 is the dashboard's declared minimum; the full-frame picker
    // must live inside it, both panes and the picture included. The
    // details column is narrow there, so a long vendor wraps with a
    // hanging indent rather than truncating --- both halves pinned.
    let frame = render(&mut app, 80, 32);
    assert!(frame.contains("nrf52840dk/nrf52840"), "the list:\n{frame}");
    assert!(
        frame.contains("Vendor Nordic Semiconductor"),
        "the vendor's first row:\n{frame}"
    );
    assert!(frame.contains("ASA"), "the vendor's wrapped tail:\n{frame}");
}

#[test]
fn without_a_transport_the_picker_stays_fully_offline() {
    let mut app = picker_app("offline");
    open_board_picker(&mut app);
    let loaded = pump_until(
        &mut app,
        |app| {
            matches!(
                app.build.as_ref().unwrap().boards.state,
                chiptui::build::ListState::Loaded(_)
            )
        },
        10,
    );
    assert!(loaded);

    let frame = render(&mut app, 100, 32);
    assert!(
        frame.contains("docs unavailable"),
        "no transport is a named state, not a spinner:\n{frame}"
    );
    assert!(
        frame.contains("nrf52840dk/nrf52840"),
        "the west list works regardless:\n{frame}"
    );
}
