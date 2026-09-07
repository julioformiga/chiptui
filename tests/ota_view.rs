//! The OTA modal end to end through the `App`: the one door (`Flash`'s
//! how-does-it-get-there menu), the one-decision button's label under each
//! reachable state, the requirement gate with the install hint, the address
//! entry replacing and restoring the modal, and the destructive confirms
//! quoting the literal command.
//!
//! The update cycle itself (stage order, failures, the halt) lives in
//! `tests/ota_update.rs`; here the fixture runs under the app's own wiring.

#![cfg(unix)]

use std::path::PathBuf;

use chiptui::app::{App, Overlay};
use chiptui::backend::BackendKind;
use chiptui::ota::update::OtaConfirm;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{click, fake, key, pump_until, render};

/// A buildable Zephyr project with a board answered (through the build
/// directory's cache), its serial/dev and home scans pointed at fixtures.
/// The address this test's fixture board answers on.
///
/// The fixture keys a board's state by the address it was reached at, so
/// the address --- not the tag --- is what [`ota_app`] has to clean, and it
/// has to be unique per test *and* per run: a `/tmp/chiptui-fake-smpmgr-…`
/// left by an earlier run is a board that is already swapped and confirmed,
/// and `the_cycle_halts_unconfirmed_and_confirms_on_a_separate_yes` then
/// waits fifteen seconds for a halt that happened before it started. The
/// pid is what makes each run's boards its own.
/// A `zephyr.dts` carrying the three nodes an A/B layout needs, in the
/// shape `devicetree::parse` reads --- the same fixture
/// `tests/ota_prepare.rs` uses, and for the same reason: the path
/// annotations are load-bearing, since only a node under `/partitions/`
/// counts.
const DTS_WITH_SLOTS: &str = "\
/* node '/soc/flash@0/partitions' defined in board.dtsi:10 */
partitions {
        /* node '/soc/flash@0/partitions/partition@0' defined in board.dtsi:13 */
        boot_partition: partition@0 {
                label = \"mcuboot\";
                reg = < 0x0 0x10000 >;
        };
        /* node '/soc/flash@0/partitions/partition@20000' defined in board.dtsi:25 */
        slot0_partition: partition@20000 {
                label = \"image-0\";
                reg = < 0x20000 0x1c0000 >;
        };
        /* node '/soc/flash@0/partitions/partition@1e0000' defined in board.dtsi:31 */
        slot1_partition: partition@1e0000 {
                label = \"image-1\";
                reg = < 0x1e0000 0x1c0000 >;
        };
};
";

fn address(tag: &str) -> String {
    let octet = tag.bytes().fold(7u32, |acc, byte| {
        (acc.wrapping_mul(31).wrapping_add(u32::from(byte))) % 250
    });
    format!("10.77.{}.{}", std::process::id() % 250, octet + 1)
}

fn ota_app(tag: &str, tool: &str) -> (App, PathBuf) {
    let root = std::env::temp_dir().join(format!("chiptui-otaview-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{}", address(tag)));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "find_package(Zephyr REQUIRED)\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("build/zephyr")).unwrap();
    std::fs::write(
        root.join("build/zephyr/CMakeCache.txt"),
        "CACHED_BOARD:STRING=xiao_esp32c3\n",
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
    app.set_ota_tool(fake(tool));
    (app, root)
}

/// The modal's one door: `Flash`'s menu, reached from any pane with `x`.
///
/// Nothing is on the fixture's USB bus, so the wired row is dimmed and the
/// cursor opens on the OTA row --- the reflex `Enter` is the modal.
fn open_modal(app: &mut App) {
    app.handle(key(KeyCode::Char('x')));
    assert!(
        matches!(app.overlay, Some(Overlay::FlashMethod { selected: 1, .. })),
        "the flash menu opens on the row that can run: {:?}",
        app.overlay
    );
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "the OTA row opens the modal"
    );
}

fn open_and_probe(app: &mut App) {
    open_modal(app);
    let probed = pump_until(
        app,
        |app| {
            app.ota.as_ref().is_some_and(|panel| {
                panel
                    .prepare
                    .requirements
                    .iter()
                    .all(|state| state.probe != chiptui::ota::prepare::ToolProbe::Probing)
            })
        },
        10,
    );
    assert!(probed, "the requirement probe answers");
}

/// `r` re-probes the requirements (the installer's grammar); the render
/// must wait for the answer, or the button sits at Blocked.
fn recheck(app: &mut App) {
    app.handle(key(KeyCode::Char('r')));
    let answered = pump_until(
        app,
        |app| {
            app.ota.as_ref().is_some_and(|panel| {
                panel
                    .prepare
                    .requirements
                    .iter()
                    .all(|state| state.probe != chiptui::ota::prepare::ToolProbe::Probing)
            })
        },
        10,
    );
    assert!(answered, "the re-probe answers");
}

/// Prepares the project for real through the modal's own confirm.
fn prepare_through_modal(app: &mut App) {
    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmOta {
            what: OtaConfirm::Prepare,
            ..
        })
    ));
    app.handle(key(KeyCode::Char('y')));
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "the modal is handed back"
    );
    assert!(
        app.ota
            .as_ref()
            .is_some_and(|panel| panel.prepare.next_step().is_none()),
        "the prepare settled: {:?}",
        app.ota.as_ref().unwrap().prepare.steps
    );
}

/// Adds the build the prepare implies: a sysbuild build directory with the
/// signed application image.
fn build_signed_image(root: &std::path::Path) {
    let app_dir = root.join("build/app/zephyr");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        root.join("build/domains.yaml"),
        "default: app\nbuild_dir: /build\ndomains:\n  - name: app\n    build_dir: /build/app\nflash_order:\n  - app\n",
    )
    .unwrap();
    std::fs::write(app_dir.join("zephyr.signed.bin"), b"signed image").unwrap();
}

/// A host target beside the board's, the shape the reference project has:
/// `build_sim/` with a `native_sim` cache and no `domains.yaml`.
fn add_simulator_variant(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("build_sim/zephyr")).unwrap();
    std::fs::write(
        root.join("build_sim/zephyr/CMakeCache.txt"),
        "CACHED_BOARD:STRING=native_sim/native/64\n",
    )
    .unwrap();
}

/// The modal follows the **board's** build directory, never the session's
/// last build --- `BuildPanel::flash_build_dir`'s rule, and for its reason:
/// a host build produces an executable and no bootloader swaps one. Reading
/// `build_dir` instead left a project with a `native_sim` target stuck on
/// `Build first` for as long as the last build had been the simulator,
/// however many times the board was rebuilt: `build_sim/` has no
/// `domains.yaml`, so no signed image ever resolves out of it.
#[test]
fn the_modal_reads_the_boards_build_directory_not_the_simulators() {
    let (mut app, root) = ota_app("hostvariant", "smpmgr");
    build_signed_image(&root);
    add_simulator_variant(&root);
    app.refresh_variants();

    // Land the session on the host variant, the way answering the build
    // target dialog with the simulator does.
    let panel = app.build.as_mut().expect("a build panel");
    let sim = panel
        .variants
        .iter()
        .position(|variant| variant.is_simulator())
        .expect("the simulator variant was discovered");
    panel.select_variant(sim);
    assert_eq!(panel.build_dir, "build_sim");

    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    let frame = render(&mut app, 100, 36);
    assert!(
        !frame.contains("Build first"),
        "the board's image is right there:\n{frame}"
    );
    assert!(
        frame.contains("Set the address"),
        "it moves on to the one question left:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Writing firmware is one door. The modal used to have three of its own
/// --- an `o` key, the last row of a workspace menu, and the help launcher
/// replaying that key --- none of which the Actions pane pointed at, while
/// the `Flash` row beside them wrote firmware without ever mentioning that
/// the other way existed.
#[test]
fn the_flash_menu_is_the_modals_one_door() {
    let (mut app, root) = ota_app("doors", "smpmgr");

    open_modal(&mut app);
    app.handle(key(KeyCode::Esc));

    // The doors that used to exist are gone. `o` is a plain letter again.
    app.handle(key(KeyCode::Char('o')));
    assert_eq!(app.overlay, None, "the `o` key went with the second door");

    // And the Zephyr Actions menu ends at the HTML dashboard: its old fifth
    // row is not there to be walked onto.
    app.overlay = Some(Overlay::ZephyrActions { selected: 3 });
    app.handle(key(KeyCode::Down));
    assert_eq!(
        app.overlay,
        Some(Overlay::ZephyrActions { selected: 0 }),
        "four rows, wrapping back to the first"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The file panes' two-cell icons used to eat the modal's left border.
///
/// The modal was `width - 4` wide, so at every width up to 100 it opened at
/// column 2 --- the *second* cell of the `📁` the Files pane draws at
/// column 1. `Clear` blanks what a popup covers, but ratatui skips the cell
/// a wide glyph covers, so the border column there was never drawn and the
/// emoji spilled across it. The fix is both halves: the wide modals share
/// the board picker's width (opening at column 1, where the popup owns the
/// glyph outright) and every popup repairs a straddling glyph one column
/// out (`ui::clear_straddling_glyphs`), so no width and no pane behind can
/// bring it back.
#[test]
fn the_modals_left_border_survives_the_file_panes_icons() {
    let (mut app, root) = ota_app("borders", "smpmgr");
    open_modal(&mut app);

    // The modal now spans the body bar one column each side, so column 1
    // *is* character 1 of every row: nothing wide precedes it to shift the
    // count. The repair itself --- erasing a glyph that straddles any
    // popup's edge, at any width --- is pinned in `ui`'s own unit tests,
    // where it can be stated without a frame around it.
    for width in [80u16, 100, 120] {
        let frame = render(&mut app, width, 32);
        let title = frame
            .lines()
            .position(|line| line.contains(" OTA "))
            .expect("the modal's title row");
        for (offset, line) in frame.lines().enumerate().skip(title).take(6) {
            // `TestBackend` quotes each row, and drops the cells a wide
            // glyph hides --- which is the very failure this pins, so the
            // quote is stripped and nothing else is assumed about the
            // count.
            let cells = line.trim_start_matches('"');
            let column = cells.chars().nth(1).expect("a second column");
            assert!(
                matches!(column, '╭' | '│' | '╰'),
                "row {offset} at {width} must carry the modal's left border, not '{column}':\n{frame}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_smpmgr_blocks_with_the_install_hint() {
    let (mut app, root) = ota_app("missing", "smpmgr-does-not-exist");
    open_and_probe(&mut app);

    let frame = render(&mut app, 100, 36);
    assert!(frame.contains("not found"), "the row says so:\n{frame}");
    assert!(
        frame.contains("pipx install smpmgr"),
        "the hint, never an install:\n{frame}"
    );
    // The button is the dimmed forward action, and pressing it is a no-op.
    assert!(
        frame.contains("Update"),
        "the nominal action, dimmed:\n{frame}"
    );
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "a blocked button does nothing"
    );
    assert!(
        !root.join("sysbuild.conf").exists(),
        "and nothing was written"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_button_labels_follow_the_one_decision() {
    let (mut app, root) = ota_app("labels", "smpmgr");
    open_and_probe(&mut app);

    // Unprepared: Prepare.
    let frame = render(&mut app, 100, 36);
    assert!(frame.contains("▶  Prepare"), "the prepare offer:\n{frame}");
    prepare_through_modal(&mut app);

    // Prepared, no signed image: the button *is* the pristine rebuild that
    // writes one, worded the way the state line already words the fix. It
    // used to be a dim `Build first` whose remedy lived in another pane.
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("Rebuild (pristine)"),
        "instrumented, no image:\n{frame}"
    );
    // And the line still names the directory it looked in, which is what
    // tells "not a sysbuild build directory" apart from "not built yet".
    assert!(
        frame.contains("no signed image in build/"),
        "the state line explains, and says where:\n{frame}"
    );

    // Image built, no address: Set the address.
    build_signed_image(&root);
    recheck(&mut app);
    let frame = render(&mut app, 100, 36);
    assert!(frame.contains("Set the address"), "no address:\n{frame}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_address_entry_replaces_and_restores_the_modal() {
    let (mut app, root) = ota_app("addr", "smpmgr");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    build_signed_image(&root);
    recheck(&mut app);

    // Enter on SetAddress opens the entry *over* the modal's slot.
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::OtaAddress { .. })),
        "the entry replaces the modal"
    );
    let addr = address("addr");
    for ch in addr.chars() {
        app.handle(key(KeyCode::Char(ch)));
    }
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "either answer hands the modal back"
    );
    // Recorded, and the button moved on to Update.
    let toml = std::fs::read_to_string(root.join("chiptui.toml")).unwrap();
    assert!(toml.contains(&format!("address = \"{addr}\"")), "{toml}");
    let frame = render(&mut app, 100, 36);
    assert!(frame.contains("▶  Update"), "everything is there:\n{frame}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_update_confirm_quotes_the_literal_smpmgr_line() {
    let (mut app, root) = ota_app("quote", "smpmgr");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    build_signed_image(&root);
    recheck(&mut app);
    app.handle(key(KeyCode::Enter));
    let addr = address("quote");
    for ch in addr.chars() {
        app.handle(key(KeyCode::Char(ch)));
    }
    app.handle(key(KeyCode::Enter));

    app.handle(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmOta {
            what: OtaConfirm::Update,
            ..
        })
    ));
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains(&format!("smpmgr --ip {addr} image upload")),
        "the literal command, not a summary:\n{frame}"
    );
    assert!(frame.contains("Push this image over the air?"), "{frame}");
    assert!(
        frame.contains(&format!("xiao_esp32c3 at {addr}")),
        "the target names board and address:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_cycle_halts_unconfirmed_and_confirms_on_a_separate_yes() {
    let (mut app, root) = ota_app("halt", "smpmgr");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    build_signed_image(&root);
    recheck(&mut app);
    app.handle(key(KeyCode::Enter));
    let addr = address("halt");
    for ch in addr.chars() {
        app.handle(key(KeyCode::Char(ch)));
    }
    app.handle(key(KeyCode::Enter));
    // A real swap's 45 s settle is not a test's to wait.
    app.ota
        .as_mut()
        .unwrap()
        .set_settle(std::time::Duration::ZERO);

    // Update -> confirm dialog -> yes: the cycle runs.
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Char('y')));
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "the modal is handed back"
    );
    let halted = pump_until(
        &mut app,
        |app| {
            app.ota
                .as_ref()
                .is_some_and(|panel| panel.awaiting_confirm())
        },
        15,
    );
    assert!(halted, "the cycle parks in front of Confirm");

    let frame = render(&mut app, 100, 36);
    assert!(frame.contains("✓  Confirm image"), "the button:\n{frame}");
    assert!(
        frame.contains("unconfirmed: the next reset reverts"),
        "the revert warning, never a green check:\n{frame}"
    );

    // The confirm is a separate question, then Done.
    app.handle(key(KeyCode::Enter));
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("image state-write --confirm"),
        "the confirm quotes its own command:\n{frame}"
    );
    app.handle(key(KeyCode::Char('y')));
    let done = pump_until(
        &mut app,
        |app| {
            app.ota
                .as_ref()
                .is_some_and(|panel| panel.action() == chiptui::ota::update::OtaAction::Done)
        },
        10,
    );
    assert!(done, "the confirm settles");
    let frame = render(&mut app, 100, 36);
    assert!(frame.contains("✓  Done"), "the finished cycle:\n{frame}");

    // And `Done` closes. It used to be dim and inert --- a button whose
    // word promised an action it refused to perform, with `Esc` as the only
    // way out.
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, None, "Done closes the modal");

    // It also ends the cycle, which it has to: the panel now survives the
    // overlay closing, and `action()` answers `Done` for as long as the
    // phase is `Finished` --- so closing alone would reopen on `Done` for
    // ever and put a second update out of reach for the session.
    open_modal(&mut app);
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("▶  Update"),
        "reopening starts a new cycle, not a permanent 'Done':\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stop_mid_upload_is_the_buttons_own_answer() {
    let (mut app, root) = ota_app("stop", "smpmgr-slow-upload");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    build_signed_image(&root);
    recheck(&mut app);
    app.handle(key(KeyCode::Enter));
    let addr = address("stop");
    for ch in addr.chars() {
        app.handle(key(KeyCode::Char(ch)));
    }
    app.handle(key(KeyCode::Enter));

    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Char('y')));
    let running = pump_until(
        &mut app,
        |app| {
            app.ota
                .as_ref()
                .is_some_and(|panel| panel.running_stage() == Some(chiptui::ota::OtaStage::Upload))
        },
        10,
    );
    assert!(running, "the upload is running");
    let frame = render(&mut app, 100, 36);
    assert!(frame.contains("■  Stop"), "the busy button:\n{frame}");

    app.handle(key(KeyCode::Enter));
    let stopped = pump_until(
        &mut app,
        |app| app.ota.as_ref().is_some_and(|panel| !panel.is_busy()),
        10,
    );
    assert!(stopped, "the cancel reports");
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("▶  Update"),
        "a user stop is not a failure --- the cycle resumes:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_modal_fits_the_declared_minimum() {
    let (mut app, root) = ota_app("minimum", "smpmgr");
    open_and_probe(&mut app);
    let frame = render(&mut app, 80, 32);
    for needle in [
        "Target",
        "Image",
        "Address",
        "Requirements",
        "smpmgr",
        "Prepare",
        "VERSION",
        "Update",
        "Verify",
        "Confirm",
        "Output",
    ] {
        assert!(
            frame.contains(needle),
            "80x32 must show '{needle}':\n{frame}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The A/B slot precondition gates the button, so it has to be *drawn*.
///
/// It was specified with three states and built with none: `action()`
/// returned `Blocked` on a known-absent layout while
/// `ui::ota` rendered only the `smpmgr` requirement, so a blocked panel
/// showed a green checklist, a dim button, and a state line pointing at a
/// row that read fine. `r` re-checked the slots and nothing on screen
/// could change.
#[test]
fn the_slot_precondition_is_drawn_and_names_itself_when_it_blocks() {
    let (mut app, root) = ota_app("slots", "smpmgr");
    open_and_probe(&mut app);

    // No build to read: not checked, and deliberately not blocking ---
    // refusing here would be ChipTUI asserting slots it cannot see.
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("slot0/slot1") && frame.contains("not checked"),
        "the unchecked state has a row of its own:\n{frame}"
    );
    assert!(
        frame.contains("▶  Prepare"),
        "and it does not block:\n{frame}"
    );

    // A devicetree with the slots: found, named by where it was read. The
    // `/* node '<path>' */` annotations are load-bearing --- only a node
    // under `/partitions/` counts (`tests/ota_prepare.rs`'s fixture shape).
    std::fs::write(root.join("build/zephyr/zephyr.dts"), DTS_WITH_SLOTS).unwrap();
    recheck(&mut app);
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("slot0/slot1") && frame.contains("found in build/zephyr/zephyr.dts"),
        "a resolved layout says where it read it:\n{frame}"
    );

    // And one without them blocks, with the row as the explanation and a
    // state line that names the slot rather than a requirement that is fine.
    std::fs::write(
        root.join("build/zephyr/zephyr.dts"),
        "/* node '/soc/flash@0' defined in board.dtsi:4 */\nflash@0 {\n};\n",
    )
    .unwrap();
    recheck(&mut app);
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("slot1_partition"),
        "the missing node is named on its row:\n{frame}"
    );
    assert!(
        frame.contains("no A/B slots --- see slot0/slot1"),
        "and the state line points at that row, not at Requirements:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A retry names the stage it retries. One shared `Retry` labelled itself
/// `"Update"` while routing to whichever question had failed, so a failed
/// `Confirm` showed a button reading "Update" that asked "Confirm the
/// running image?" --- the button's word contradicting its effect, which
/// is the exact failure `install::Action`'s rule exists to prevent.
#[test]
fn a_failed_confirm_offers_the_confirm_again_not_an_update() {
    use chiptui::ota::OtaStage;
    use chiptui::stepper::StepState;

    let (mut app, root) = ota_app("retryconfirm", "smpmgr");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    build_signed_image(&root);
    recheck(&mut app);

    // Put the panel where a verified swap leaves it, then fail the confirm.
    let panel = app.ota.as_mut().expect("a panel");
    let confirm = panel
        .stage_list()
        .iter()
        .position(|stage| *stage == OtaStage::Confirm)
        .unwrap();
    panel.mark_awaiting_confirm_for_test();
    panel.fail_stage_for_test(confirm, "the board refused".to_string());

    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("✓  Confirm image"),
        "the button names what it retries:\n{frame}"
    );
    assert!(
        !frame.contains("▶  Update"),
        "and never the sequence it happens to live in:\n{frame}"
    );
    // Both facts are true at once and the line carries both, with the
    // warning first so a long stage reason cannot truncate it away. It used
    // to show only the halt, hiding why the confirm had not taken.
    assert!(
        frame.contains("unconfirmed --- Confirm: the board refused"),
        "the reason no longer hides behind the halt:\n{frame}"
    );

    // And pressing it asks the confirm's own question.
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::ConfirmOta {
                what: OtaConfirm::ConfirmImage,
                ..
            })
        ),
        "the dialog matches the word on the button: {:?}",
        app.overlay
    );
    assert_eq!(
        app.ota.as_ref().unwrap().stages[confirm],
        StepState::Failed("Confirm: the board refused".to_string())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The unconfirmed halt survives the window closing.
///
/// `open_ota` built a fresh panel every time and `Esc` dropped the old one,
/// and `OtaPanel::new` starts with `awaiting_confirm: false` --- so one
/// `Esc` discarded the single state the whole mechanism exists to surface,
/// and the reopened modal offered to update a board whose running image
/// would revert on the next reset.
#[test]
fn closing_and_reopening_keeps_the_unconfirmed_halt() {
    let (mut app, root) = ota_app("reopen", "smpmgr");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    build_signed_image(&root);
    recheck(&mut app);
    app.ota
        .as_mut()
        .expect("a panel")
        .mark_awaiting_confirm_for_test();

    let frame = render(&mut app, 100, 36);
    assert!(frame.contains("unconfirmed"), "the halt is up:\n{frame}");

    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
    open_modal(&mut app);

    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("✓  Confirm image") && frame.contains("unconfirmed"),
        "the reopened modal still says the image is unconfirmed:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// With no address the driver refuses by name, and that reason belongs on
/// the rows. `stage_command` used to swallow it with `.ok()`, so all seven
/// read "waiting on an earlier stage" --- untrue: nothing was waiting on a
/// stage.
#[test]
fn the_stage_rows_name_the_missing_address_instead_of_blaming_each_other() {
    let (mut app, root) = ota_app("noaddr", "smpmgr");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    build_signed_image(&root);
    recheck(&mut app);

    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("no IP address configured"),
        "the driver's own refusal reaches the rows:\n{frame}"
    );
    assert!(
        !frame.contains("waiting on an earlier stage"),
        "and the generic placeholder is gone:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The transport decides the Kconfig block the prepare writes, and it used
/// to be answerable only by hand-editing `chiptui.toml` --- before
/// preparing, since afterwards the block is already written.
#[test]
fn the_transport_picker_records_the_answer_and_reopens_the_question() {
    let (mut app, root) = ota_app("transport", "smpmgr");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);

    app.handle(key(KeyCode::Char('t')));
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("UDP (current)") && frame.contains("serial") && frame.contains("BLE"),
        "the picker opens on the current answer:\n{frame}"
    );

    // Down to serial, accept.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::Ota)), "handed back");

    let config = std::fs::read_to_string(root.join("chiptui.toml")).unwrap();
    assert!(
        config.contains("transport = \"serial\""),
        "the answer is recorded: {config}"
    );
    // The board fragment's body is keyed off the transport, so the prepare
    // has work again --- no special casing anywhere.
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("▶  Prepare"),
        "the stale Kconfig block reopens the prepare:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `p` runs the driver's probe alone. The address is typed by hand and
/// nothing validates it, and the probe used to run only inside the
/// destructive update confirm --- so the only way to find out the address
/// was wrong was to agree to push an image to it.
#[test]
fn p_probes_the_board_without_committing_to_the_cycle() {
    let (mut app, root) = ota_app("probe", "smpmgr");
    open_and_probe(&mut app);
    prepare_through_modal(&mut app);
    build_signed_image(&root);
    recheck(&mut app);
    let addr = address("probe");
    app.ota.as_mut().unwrap().set_address(addr.clone()).unwrap();

    app.handle(key(KeyCode::Char('p')));
    // No confirm stood in the way: a read needs none.
    assert!(
        matches!(app.overlay, Some(Overlay::Ota)),
        "no dialog opened"
    );
    let done = pump_until(
        &mut app,
        |app| {
            app.ota
                .as_ref()
                .is_some_and(|panel| panel.running_stage().is_none())
        },
        60,
    );
    assert!(done, "the probe settles");

    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("os echo chiptui"),
        "the probe ran and its command is on screen:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{addr}"));
}

/// The footer button answers a click anywhere on the box that is *drawn*.
///
/// The renderer draws it half the modal wide; the hit-testing sized it at
/// `STOP_BOX_WIDTH`'s thirteen columns, so most of the visible button did
/// nothing. Both sides read one helper now --- the rule
/// `ui::layout::overlay_popup` keeps for every other overlay, and the
/// reason it exists.
#[test]
fn a_click_on_the_drawn_footer_button_presses_it() {
    let (mut app, root) = ota_app("click", "smpmgr");
    app.set_mouse_enabled(true);
    open_and_probe(&mut app);

    let frame = render(&mut app, 100, 36);
    let lines: Vec<&str> = frame.lines().collect();
    let row = lines
        .iter()
        .position(|line| line.contains("▶  Prepare"))
        .expect("the button is drawn") as u16;
    // Drawn columns, never byte offsets: the borders are multi-byte.
    let col = lines[row as usize]
        .chars()
        .position(|ch| ch == '▶')
        .expect("the glyph is on the row") as u16;

    // The glyph sits near the box's *left* edge --- the half the old
    // thirteen-column rect could never reach.
    app.handle(chiptui::event::AppEvent::Mouse(click(col, row)));
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::ConfirmOta {
                what: OtaConfirm::Prepare,
                ..
            })
        ),
        "the click pressed the button: {:?}",
        app.overlay
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The transport row: the one failure in this flow with no symptom of its
/// own.
///
/// `MCUMGR_TRANSPORT_*` is a `depends on`, so an unmet dependency drops it
/// to `n` and every other signal still reports success --- the board just
/// never answers, which reads like a wrong address or dead hardware. The
/// row reads the built `.config` back and says which it is.
#[test]
fn the_transport_row_reports_what_the_build_settled_on() {
    let (mut app, root) = ota_app("transportrow", "smpmgr");
    open_and_probe(&mut app);

    // No build: not checked, and never blocking.
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("transport") && frame.contains("not checked"),
        "the unchecked state has a row:\n{frame}"
    );

    prepare_through_modal(&mut app);

    // A build that carries it.
    let dir = root.join("build/zephyr");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".config"),
        "CONFIG_NET_UDP=y\nCONFIG_MCUMGR_TRANSPORT_UDP=y\n",
    )
    .unwrap();
    recheck(&mut app);
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("CONFIG_MCUMGR_TRANSPORT_UDP=y in build/zephyr/.config"),
        "an enabled transport names where it read it:\n{frame}"
    );

    // And one that dropped it, with the unmet dependencies named --- the
    // whole help there is to give, since the network stack is the
    // project's own architecture.
    std::fs::write(dir.join(".config"), "CONFIG_FOO=y\n").unwrap();
    recheck(&mut app);
    let frame = render(&mut app, 100, 36);
    assert!(
        frame.contains("dropped by the build --- needs NET_UDP and NET_SOCKETS"),
        "the silent failure gets a name, and the unmet dependencies with \
         it --- both inside the row's width:\n{frame}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
