//! The OTA update cycle end to end, against the fixture `smpmgr`: the
//! stages run in the driver's declared order, the hash the read answers is
//! what the mark names, the runner halts in front of `Confirm` with the
//! revert named, a failed stage stops and offers `Retry`, `Stop` mid-upload
//! cancels, and a verify that reports the *old* hash is a named failure ---
//! never a silent pass.
//!
//! Driven the way `tests/ota_prepare.rs` drives the prepare: the panel and
//! a `ProcessManager` directly, no `App` (that wiring is `ota_view.rs`'s).
//! The fixture's board is per-test via the address, so parallel tests never
//! share one.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chiptui::ota::update::{OtaAction, OtaPanel};
use chiptui::ota::{OtaConfig, OtaStage};
use chiptui::process::ProcessManager;
use chiptui::stepper::{Phase, StepState};

mod common;
use common::fake;

const BOARD: &str = "xiao_esp32c3";
const NEW_HASH: &str = "AABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDDAABBCCDD";

fn temp_project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chiptui-ota-update-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A panel over an *instrumented* project with a signed image built: the
/// prepare has already run (driven for real --- the files it writes are
/// what the update then reads) and the build directory holds the sysbuild
/// outputs. `address` is the test's own fixture board.
fn ready_panel(tag: &str, tool: &str, address: &str) -> (OtaPanel, ProcessManager, PathBuf) {
    let root = temp_project(tag);
    let config = OtaConfig {
        address: Some(address.to_string()),
        ..OtaConfig::default()
    };
    let mut panel = OtaPanel::new(&root, BOARD, Some("build".to_string()), config).unwrap();
    panel.set_tool(fake(tool));
    panel.set_settle(Duration::ZERO);
    // The prepare runs for real; its probe is satisfied directly (the
    // fixture's `--version` answer is `ota_prepare.rs`'s business).
    panel.prepare.requirements.iter_mut().for_each(|state| {
        state.probe = chiptui::ota::prepare::ToolProbe::Present("0.19.0".to_string());
    });
    assert!(
        panel.start_prepare().finished,
        "prepare settles: {:?}",
        panel.prepare.steps
    );

    // The rebuild the prepare implies: a sysbuild build directory with the
    // signed application image.
    let app_dir = root.join("build/app/zephyr");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        root.join("build/domains.yaml"),
        "default: app\nbuild_dir: /build\ndomains:\n  - name: app\n    build_dir: /build/app\nflash_order:\n  - app\n",
    )
    .unwrap();
    std::fs::write(app_dir.join("zephyr.signed.bin"), b"signed image").unwrap();
    panel.refresh_image();
    assert_eq!(panel.action(), OtaAction::Update, "everything is there");

    (panel, ProcessManager::new(), root)
}

/// Drains process events (and the settle-driving tick) until `ready` or the
/// deadline.
fn pump(
    panel: &mut OtaPanel,
    processes: &mut ProcessManager,
    secs: u64,
    ready: impl Fn(&OtaPanel) -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for event in processes.drain() {
            panel.on_process(&event, processes);
        }
        panel.tick(processes);
        if ready(panel) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// The fixture's invocation log for this test's board.
fn fixture_log(address: &str) -> String {
    std::fs::read_to_string(format!("/tmp/chiptui-fake-smpmgr-{address}/log")).unwrap_or_default()
}

#[test]
fn the_cycle_runs_the_declared_stages_and_halts_unconfirmed() {
    let address = "10.99.0.1";
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
    let (mut panel, mut processes, root) = ready_panel("cycle", "smpmgr", address);

    assert!(panel.start_update(&mut processes));
    let halted = pump(&mut panel, &mut processes, 15, |panel| {
        panel.awaiting_confirm()
    });
    assert!(halted, "the cycle parks in front of Confirm");

    // The declared order, exactly --- read off the fixture's own log.
    let log = fixture_log(address);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines,
        vec![
            "os echo chiptui",
            &format!(
                "image upload {}",
                root.join("build/app/zephyr/zephyr.signed.bin").display()
            ),
            "image state-read",
            &format!("image state-write {NEW_HASH}"),
            "os reset",
            "image state-read",
        ],
        "probe, upload, read, mark, reset, verify --- and no confirm"
    );

    // The hash the read answered is the one the mark wrote.
    assert_eq!(panel.slot_hash(), Some(NEW_HASH));

    // The halt is never a silent success: the panel parks in front of the
    // confirm, which is a separate decision.
    assert_eq!(panel.action(), OtaAction::ConfirmImage);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
}

#[test]
fn the_confirm_is_a_separate_step_that_completes_the_cycle() {
    let address = "10.99.0.2";
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
    let (mut panel, mut processes, root) = ready_panel("confirm", "smpmgr", address);
    assert!(panel.start_update(&mut processes));
    assert!(pump(&mut panel, &mut processes, 15, |panel| panel
        .awaiting_confirm()));

    assert!(panel.confirm_image(&mut processes));
    let done = pump(&mut panel, &mut processes, 10, |panel| {
        matches!(panel.update_phase, Phase::Finished)
    });
    assert!(done, "the confirm settles: {:?}", panel.stages.last());
    assert_eq!(panel.action(), OtaAction::Done);
    assert!(
        fixture_log(address).lines().last() == Some("image state-write --confirm"),
        "the last invocation is the confirm"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
}

#[test]
fn a_failed_stage_stops_the_cycle_and_offers_retry() {
    let address = "10.99.0.3";
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
    let (mut panel, mut processes, root) = ready_panel("retry", "smpmgr-fail-upload", address);

    assert!(panel.start_update(&mut processes));
    let failed = pump(&mut panel, &mut processes, 15, |panel| {
        matches!(panel.update_phase, Phase::Stopped(_))
    });
    assert!(failed, "the dead link stops the cycle");

    let upload = panel
        .stage_list()
        .iter()
        .position(|stage| *stage == OtaStage::Upload)
        .unwrap();
    assert!(
        matches!(&panel.stages[upload], StepState::Failed(reason) if reason.contains("Upload")),
        "the failure names its stage: {:?}",
        panel.stages[upload]
    );
    assert_eq!(panel.action(), OtaAction::RetryUpdate);
    // The stages after the upload never ran.
    let read = panel
        .stage_list()
        .iter()
        .position(|stage| *stage == OtaStage::ReadState)
        .unwrap();
    assert_eq!(panel.stages[read], StepState::Pending);
    // The retry's question comes off the action itself now, so the
    // button's word and the dialog behind it cannot disagree.
    assert_eq!(
        panel.action().confirm(),
        Some(chiptui::ota::update::OtaConfirm::Update)
    );
    assert_eq!(panel.action().label(), "Update");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
}

#[test]
fn stop_mid_upload_cancels_and_resumes_later() {
    let address = "10.99.0.4";
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
    let (mut panel, mut processes, root) = ready_panel("stop", "smpmgr-slow-upload", address);

    assert!(panel.start_update(&mut processes));
    let uploading = pump(&mut panel, &mut processes, 10, |panel| {
        panel.running_stage() == Some(OtaStage::Upload)
    });
    assert!(uploading, "the upload is running");

    assert!(panel.stop(&mut processes));
    let settled = pump(&mut panel, &mut processes, 10, |panel| !panel.is_busy());
    assert!(settled, "the cancel reports");

    // A user stop is not a failure: the stage is pending again, and the
    // next `Update` resumes from it.
    let upload = panel
        .stage_list()
        .iter()
        .position(|stage| *stage == OtaStage::Upload)
        .unwrap();
    assert_eq!(panel.stages[upload], StepState::Pending);
    assert_eq!(panel.action(), OtaAction::Update);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
}

#[test]
fn a_verify_reporting_the_old_hash_is_a_named_failure() {
    let address = "10.99.0.5";
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
    let (mut panel, mut processes, root) = ready_panel("noswap", "smpmgr-no-swap", address);

    assert!(panel.start_update(&mut processes));
    let failed = pump(&mut panel, &mut processes, 15, |panel| {
        matches!(panel.update_phase, Phase::Stopped(_))
    });
    assert!(failed, "a board that comes back unchanged fails the cycle");

    let verify = panel
        .stage_list()
        .iter()
        .position(|stage| *stage == OtaStage::Verify)
        .unwrap();
    match &panel.stages[verify] {
        StepState::Failed(reason) => {
            assert!(
                reason.contains("the swap did not take"),
                "the failure says what happened: {reason}"
            );
        }
        other => panic!("Verify must fail by name, not {other:?}"),
    }
    assert!(
        !panel.awaiting_confirm(),
        "never parked in front of Confirm"
    );
    assert_eq!(panel.action(), OtaAction::RetryUpdate);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
}

/// Stopping the post-reset settle is a stop, and must read as one.
///
/// It used to set `Phase::Idle`, so `action()` fell through to `Update`
/// and the state line claimed "the board answers, the image is signed" ---
/// said of a board that is very likely still mid-swap and unreachable. The
/// only honest note was a line in the scrolling output.
#[test]
fn stopping_the_settle_says_so_and_resumes_at_verify() {
    let address = "10.99.0.7";
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
    let (mut panel, mut processes, root) = ready_panel("settlestop", "smpmgr", address);
    // Long enough that the settle is still running when we stop it.
    panel.set_settle(Duration::from_secs(30));

    assert!(panel.start_update(&mut processes));
    let settling = pump(&mut panel, &mut processes, 15, |panel| {
        panel.settling_remaining().is_some()
    });
    assert!(settling, "the cycle reaches the post-reset settle");

    assert!(panel.stop(&mut processes));
    assert!(
        matches!(&panel.update_phase, Phase::Stopped(reason)
            if reason.contains("ended early") && reason.contains("still be swapping")),
        "the stop names itself instead of reading as ready: {:?}",
        panel.update_phase
    );
    // And the button resumes rather than offering a fresh push.
    assert_eq!(panel.action(), OtaAction::RetryUpdate);
    let (stage, _) = panel.next_stage_command().expect("a stage to resume at");
    assert_eq!(
        stage,
        OtaStage::Verify,
        "the swap already happened; verifying is the honest next question"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
}

/// The probe runs alone. The address is hand-typed and unvalidated, and the
/// probe used to be reachable only from inside the destructive update
/// confirm --- so the only way to learn the address was wrong was to agree
/// to push an image to it.
#[test]
fn a_probe_runs_alone_and_leaves_the_rest_of_the_cycle_pending() {
    let address = "10.99.0.8";
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
    let (mut panel, mut processes, root) = ready_panel("probealone", "smpmgr", address);

    assert!(panel.probe_board(&mut processes));
    let done = pump(&mut panel, &mut processes, 15, |panel| {
        panel.running_stage().is_none()
    });
    assert!(done, "the probe settles");

    let probe = panel
        .stage_list()
        .iter()
        .position(|stage| *stage == OtaStage::Probe)
        .unwrap();
    assert_eq!(panel.stages[probe], StepState::Done);
    // Everything after it is untouched: a probe asks about the board, it
    // does not start the cycle.
    for (index, stage) in panel.stage_list().iter().enumerate() {
        if index == probe {
            continue;
        }
        assert_eq!(
            panel.stages[index],
            StepState::Pending,
            "{stage:?} must not have run"
        );
    }
    // And the board saw exactly one command.
    let log = fixture_log(address);
    let lines: Vec<&str> = log.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one command only: {lines:?}");
    assert!(
        lines[0].contains("os echo"),
        "and it is the probe: {lines:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(format!("/tmp/chiptui-fake-smpmgr-{address}"));
}
