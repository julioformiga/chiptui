//! The Zephyr installer end to end: the prerequisite checklist and its
//! gate, the getting-started sequence run through the process manager, the
//! resumption of a half-finished workspace, and what a finished
//! installation writes.
//!
//! Nothing here touches real tools. Every command is answered by a fixture
//! in `tests/fixtures/bin/`, resolved by absolute path so no test mutates
//! `PATH`. The fixtures do the small amount of real filesystem work their
//! step is detected by (`.west/config`, `zephyr/VERSION`, an SDK bundle),
//! which is what lets the resumption tests assert against the same
//! evidence a real machine would leave.
//!
//! Every fixture app calls `set_home_dir` before `bootstrap`: a test that
//! forgets writes into the developer's real `~/.config/chiptui/config.toml`.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use chiptui::app::{App, Overlay};
use chiptui::backend::BackendKind;
use chiptui::event::AppEvent;
use chiptui::install::{Action, Phase, Step, StepState};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{fake, key, log_mentions, render};

fn root_for(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("chiptui-install-{tag}-{}", std::process::id()))
}

/// A Zephyr app with nothing configured --- the installer's whole reason to
/// exist --- with every prerequisite query and `pyenv` pointed at fixtures.
fn install_app(tag: &str) -> (App, PathBuf) {
    install_app_configured(tag, |_| String::new())
}

/// The same, over a machine `seed` prepared first: it runs against the
/// (freshly emptied) root and returns the user config to write, so a test
/// can stand up an existing installation and point the config at it before
/// `bootstrap` ever resolves anything.
fn install_app_configured(tag: &str, seed: impl FnOnce(&Path) -> String) -> (App, PathBuf) {
    let root = root_for(tag);
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(root.join("dev")).unwrap();
    let config = seed(&root);
    if !config.is_empty() {
        let path = home.join(".config/chiptui/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, config).unwrap();
    }

    let mut app = App::new(&root);
    app.set_serial_dir(root.join("dev"));
    app.set_home_dir(&home);
    app.bootstrap();
    app.manager.set_override(Some(BackendKind::Zephyr));
    app.maybe_scan_devices();
    for tool in ["cmake", "dtc", "pyenv", "python3"] {
        app.set_installer_tool_path(tool, fake(tool));
    }
    (app, root)
}

/// Drains process events until `ready` says the app reached the state the
/// test is waiting for. Returns whether it did before the deadline.
/// [`common::pump_until`] with this file's own argument order: the deadline
/// reads better *before* the long multi-line predicates the installer tests
/// pass, and thirteen call sites already spell it that way.
fn pump_until(app: &mut App, secs: u64, ready: impl Fn(&App) -> bool) -> bool {
    common::pump_until(app, ready, secs)
}

fn probes_done(app: &App) -> bool {
    app.installer.as_ref().is_some_and(|installer| {
        installer
            .prereqs
            .iter()
            .all(|state| state.probe != chiptui::install::Probe::Probing)
    })
}

/// A complete Zephyr installation at `dir`, as far as
/// `zephyr::workspace::install_check` is concerned: `.west/config` naming
/// the manifest path, and the checkout it names.
fn installed_workspace(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join(".west")).unwrap();
    std::fs::write(
        dir.join(".west/config"),
        "[manifest]\npath = zephyr\nfile = west.yml\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("zephyr")).unwrap();
    std::fs::write(
        dir.join("zephyr/VERSION"),
        "VERSION_MAJOR = 4\nVERSION_MINOR = 1\nPATCHLEVEL = 0\n",
    )
    .unwrap();
    // A real checkout pins the SDK version it wants, and the bundle
    // detection is version-aware --- seeding it is what makes these
    // fixtures describe the state they claim to.
    std::fs::write(dir.join("zephyr/SDK_VERSION"), "0.17.0\n").unwrap();
    dir.to_path_buf()
}

/// The SDK bundle beside a workspace --- the difference between an
/// installation that is finished and one whose SDK step never ran.
/// `toolchains` are the ones unpacked into `gnu/`, which is what
/// distinguishes a bundle that carries what the user wants from one that
/// does not.
fn installed_sdk_with(dir: &Path, toolchains: &[&str]) {
    let sdk = dir.join("zephyr-sdk-0.17.0");
    std::fs::create_dir_all(&sdk).unwrap();
    std::fs::write(sdk.join("sdk_version"), "0.17.0\n").unwrap();
    for toolchain in toolchains {
        std::fs::create_dir_all(sdk.join("gnu").join(toolchain)).unwrap();
    }
}

fn installed_sdk(dir: &Path) {
    installed_sdk_with(dir, &[]);
}

fn user_config(root: &Path) -> String {
    std::fs::read_to_string(root.join("home/.config/chiptui/config.toml")).unwrap()
}

/// Opens the installer over `parent` and waits for the checklist to fill.
fn open(app: &mut App, parent: &Path) {
    app.open_installer(parent.to_path_buf());
    assert!(
        pump_until(app, 10, probes_done),
        "the prerequisite queries must answer"
    );
}

/// Presses the modal's action button and answers its confirm with Yes,
/// answering the SDK question first when the test has not: with no
/// toolchain picked the installer deliberately refuses to start, so that
/// `west sdk install` can never fall through to its own 35-toolchain
/// default.
fn accept_confirm(app: &mut App) {
    if let Some(installer) = app.installer.as_mut()
        && !installer.sdk_ready()
    {
        installer
            .picked_toolchains
            .push("arm-zephyr-eabi".to_string());
    }
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::Confirm { .. })),
        "starting must confirm first"
    );
    app.handle(key(KeyCode::Char('y')));
}

#[test]
fn the_checklist_reports_every_prerequisite_and_the_python_row_only_warns() {
    let (mut app, root) = install_app("checklist");
    open(&mut app, &root.join("ws"));

    let installer = app.installer.as_ref().unwrap();
    let by = |name: &str| {
        installer
            .prereqs
            .iter()
            .find(|state| state.prereq.label() == name)
            .unwrap()
    };
    assert_eq!(
        by("cmake").probe.version().map(|v| v.to_string()),
        Some("3.31.2".to_string())
    );
    // dtc prints its banner on stderr; the installer reads both streams.
    assert_eq!(
        by("dtc").probe.version().map(|v| v.to_string()),
        Some("1.7.0".to_string())
    );
    assert_eq!(
        by("pyenv").probe.version().map(|v| v.to_string()),
        Some("2.4.7".to_string())
    );
    // The system Python is 3.11.9 --- not the recommended series, and not
    // a reason to stop: pyenv provides 3.12 for the workspace.
    assert_eq!(
        by("python").probe,
        chiptui::install::Probe::OffSeries(chiptui::install::Version::new(3, 11, 9))
    );
    assert!(by("python").satisfied());
    assert!(
        installer.prereqs_ready(),
        "an off-series system Python must not block the installation"
    );
    // The other gate is separate and still open here --- see
    // `the_sdk_step_never_downloads_everything_by_omission`.
    assert!(!installer.sdk_ready());

    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("Install Zephyr"),
        "the modal draws:\n{frame}"
    );
    assert!(frame.contains("3.31.2"), "cmake's version shows:\n{frame}");
    assert!(
        frame.contains("min 3.28.0"),
        "the minimum shows beside it:\n{frame}"
    );
    assert!(
        frame.contains("pyenv provides 3.12"),
        "the ⚠ python row must explain itself:\n{frame}"
    );
}

#[test]
fn a_missing_blocking_prerequisite_stops_the_sequence_and_names_the_fix() {
    let (mut app, root) = install_app("blocked");
    // A cmake below Zephyr's minimum: present, but not new enough.
    app.set_installer_tool_path("cmake", fake("cmake-old"));
    open(&mut app, &root.join("ws"));

    let installer = app.installer.as_ref().unwrap();
    assert_eq!(
        installer.prereqs[0].probe,
        chiptui::install::Probe::Old(chiptui::install::Version::new(3, 9, 6)),
        "3.9.6 is older than 3.28.0 --- as strings it sorts the other way"
    );
    assert!(!installer.prereqs_ready());
    assert!(!installer.can_start());

    // Enter is a no-op: no confirm opens, nothing spawns.
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::ZephyrInstall)),
        "a blocked installer must not open the confirm"
    );
    assert_eq!(app.installer.as_ref().unwrap().phase, Phase::Idle);

    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("3.9.6"),
        "the version found must show:\n{frame}"
    );
    assert!(
        frame.contains("install") || frame.contains("cmake.org"),
        "the row must name a way to fix it:\n{frame}"
    );
}

#[test]
fn the_sequence_installs_a_workspace_and_saves_it() {
    let (mut app, root) = install_app("sequence");
    let parent = root.join("ws");
    open(&mut app, &parent);
    let target = parent.join("zephyr");
    assert_eq!(
        app.installer.as_ref().unwrap().root,
        target,
        "the workspace goes into zephyr/ inside the picked folder"
    );

    accept_confirm(&mut app);
    assert!(
        pump_until(&mut app, 60, |app| app.installer.is_none()),
        "the sequence must run to the end"
    );

    // The guide's own result, on disk.
    assert!(target.join(".west/config").is_file());
    assert!(target.join("zephyr/VERSION").is_file());
    assert!(target.join(".venv/bin/west").is_file());
    assert_eq!(
        std::fs::read_to_string(target.join(".python-version"))
            .unwrap()
            .trim(),
        "3.12.13",
        "the newest 3.12 pyenv offers, not the 3.13 beside it"
    );
    assert!(target.join("zephyr-sdk-0.17.0/sdk_version").is_file());

    // The answer is persisted the way every environment pick is --- and
    // the SDK beside it, since the installer is what knows where it landed.
    let config = std::fs::read_to_string(root.join("home/.config/chiptui/config.toml")).unwrap();
    assert!(
        config.contains(&format!("workspace = \"{}\"", target.display())),
        "the installation must be saved:\n{config}"
    );
    assert!(
        config.contains("zephyr-sdk-0.17.0"),
        "the SDK location must be saved beside it:\n{config}"
    );

    // Resolved now --- so the environment row is `Update Zephyr` again.
    assert!(app.workspace.as_ref().unwrap().resolved.is_some());
    assert_eq!(
        app.build
            .as_ref()
            .unwrap()
            .actions(&app.manager.capabilities())
            .first(),
        Some(&chiptui::build::BuildAction::UpdateZephyr)
    );
    // And the flow moves to the next question the checklist still has open.
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::DirPicker {
                purpose: chiptui::workspace::DirPurpose::Projects,
                ..
            })
        ),
        "a finished installation chains into the projects folder question"
    );
}

#[test]
fn a_failing_step_stops_the_sequence_and_offers_a_retry() {
    let (mut app, root) = install_app("failure");
    // A `pyenv` that is not there at all: present enough for the checklist
    // (which was probed with the good fixture) but gone when the sequence
    // reaches it.
    open(&mut app, &root.join("ws"));
    app.set_installer_tool_path("pyenv", fake("pyenv-missing"));

    accept_confirm(&mut app);
    assert!(
        pump_until(&mut app, 30, |app| app
            .installer
            .as_ref()
            .is_some_and(chiptui::install::Installer::stopped)),
        "a failed step must stop the sequence"
    );

    let installer = app.installer.as_ref().unwrap();
    assert!(matches!(installer.steps[0], StepState::Failed(_)));
    assert!(
        installer.steps[1..]
            .iter()
            .all(|state| *state == StepState::Pending),
        "nothing after the failure may have run"
    );
    // The failed step is where a retry resumes.
    assert_eq!(installer.next_step(), Some(0));

    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("Retry"),
        "the button offers a retry:\n{frame}"
    );
}

#[test]
fn an_interrupted_installation_resumes_where_it_stopped() {
    let (mut app, root) = install_app("resume");
    let target = root.join("ws/zephyr");
    // A workspace that got as far as `west init` and stopped: the pin, the
    // venv with west in it, and `.west/` --- but no manifest checkout.
    std::fs::create_dir_all(target.join(".venv/bin")).unwrap();
    std::fs::create_dir_all(target.join(".west")).unwrap();
    std::fs::copy(fake("west"), target.join(".venv/bin/west")).unwrap();
    std::fs::copy(fake("pip"), target.join(".venv/bin/pip")).unwrap();
    std::fs::write(target.join(".python-version"), "3.12.13\n").unwrap();

    open(&mut app, &root.join("ws"));
    let installer = app.installer.as_ref().unwrap();
    let state =
        |step: Step| installer.steps[Step::ALL.iter().position(|s| *s == step).unwrap()].clone();
    assert_eq!(state(Step::PyenvInstall), StepState::Done);
    assert_eq!(state(Step::PyenvLocal), StepState::Done);
    assert_eq!(state(Step::Venv), StepState::Done);
    assert_eq!(state(Step::PipWest), StepState::Done);
    assert_eq!(state(Step::WestInit), StepState::Done);
    assert_eq!(
        state(Step::WestUpdate),
        StepState::Pending,
        "no manifest checkout: the update is exactly what is left"
    );
    // The queries never resume --- their answers live in memory only.
    assert_eq!(state(Step::ResolvePython), StepState::Pending);

    accept_confirm(&mut app);
    assert!(
        pump_until(&mut app, 60, |app| app.installer.is_none()),
        "resuming must reach the end"
    );
    assert!(target.join("zephyr/VERSION").is_file());
}

#[test]
fn skipping_the_sdk_leaves_the_rest_of_the_sequence_alone() {
    let (mut app, root) = install_app("skip-sdk");
    let target = root.join("ws/zephyr");
    open(&mut app, &root.join("ws"));

    app.handle(key(KeyCode::Char('s')));
    let installer = app.installer.as_ref().unwrap();
    assert!(installer.sdk_skipped);
    for (index, step) in Step::ALL.iter().enumerate() {
        if step.belongs_to_sdk() {
            assert_eq!(installer.steps[index], StepState::Skipped);
        }
    }

    accept_confirm(&mut app);
    assert!(
        pump_until(&mut app, 60, |app| app.installer.is_none()),
        "the sequence must still finish"
    );
    assert!(target.join(".west/config").is_file());
    assert!(
        !target.join("zephyr-sdk-0.17.0").exists(),
        "a skipped SDK step must install nothing"
    );
    let config = std::fs::read_to_string(root.join("home/.config/chiptui/config.toml")).unwrap();
    assert!(config.contains("workspace = "));
    assert!(
        !config.contains("sdk = "),
        "no bundle landed, so no sdk key is written:\n{config}"
    );
}

#[test]
fn the_toolchain_pick_reaches_the_sdk_command() {
    let (mut app, root) = install_app("toolchains");
    open(&mut app, &root.join("ws"));

    let sdk_index = Step::ALL
        .iter()
        .position(|step| *step == Step::SdkInstall)
        .unwrap();
    // Nothing picked would make `west sdk install` fall through to its own
    // `-t all` --- 35 toolchains --- so the installer refuses to start
    // rather than letting that be the default.
    let target = root.join("ws/zephyr");
    assert_eq!(
        app.installer
            .as_ref()
            .unwrap()
            .step_command(sdk_index)
            .unwrap()
            .to_string(),
        format!("west sdk install -b {}", target.display()),
    );
    assert!(!app.installer.as_ref().unwrap().sdk_ready());
    assert_eq!(
        app.installer.as_ref().unwrap().action(),
        Action::PickToolchains
    );

    // The names are a curated constant --- nothing can enumerate them
    // before an SDK exists --- and the title carries the SDK version the
    // checkout pins, once `west update` has left one.
    std::fs::create_dir_all(root.join("ws/zephyr/zephyr")).unwrap();
    std::fs::write(root.join("ws/zephyr/zephyr/SDK_VERSION"), "1.0.1\n").unwrap();

    app.handle(key(KeyCode::Char('t')));
    assert!(matches!(app.overlay, Some(Overlay::SdkToolchains { .. })));
    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("SDK_VERSION 1.0.1"),
        "the picker names the release it is picking for:\n{frame}"
    );
    assert!(
        frame.contains("aarch64-zephyr-elf"),
        "the picker lists the curated names:\n{frame}"
    );

    // TOOLCHAINS[3] is arm-zephyr-eabi, [15] xtensa-espressif_esp32.
    let pick = |app: &mut App, index: usize| {
        for _ in 0..index {
            app.handle(key(KeyCode::Down));
        }
        app.handle(key(KeyCode::Char(' ')));
        for _ in 0..index {
            app.handle(key(KeyCode::Up));
        }
    };
    let arm = position_of("arm-zephyr-eabi");
    let esp = position_of("xtensa-espressif_esp32_zephyr-elf");
    pick(&mut app, arm);
    pick(&mut app, esp);
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::ZephyrInstall)),
        "the picker returns to the installer it was opened from"
    );

    assert_eq!(
        app.installer.as_ref().unwrap().picked_toolchains,
        vec![
            "arm-zephyr-eabi".to_string(),
            "xtensa-espressif_esp32_zephyr-elf".to_string()
        ]
    );
    // One `-t`, every name after it, and `-t` last on the line: west
    // declares it `nargs="+"`, so a repeated flag would silently keep only
    // the final name and the greedy `+` would swallow a following option.
    assert_eq!(
        app.installer
            .as_ref()
            .unwrap()
            .step_command(sdk_index)
            .unwrap()
            .to_string(),
        format!(
            "west sdk install -b {} -t arm-zephyr-eabi xtensa-espressif_esp32_zephyr-elf",
            target.display()
        ),
        "the pick has to reach the command, and `-b` has to precede the greedy `-t`"
    );
    assert!(app.installer.as_ref().unwrap().sdk_ready());
    assert_eq!(app.installer.as_ref().unwrap().action(), Action::Install);
}

fn position_of(name: &str) -> usize {
    chiptui::install::steps::TOOLCHAINS
        .iter()
        .position(|candidate| *candidate == name)
        .unwrap_or_else(|| panic!("{name} must be in the curated list"))
}

#[test]
fn the_sdk_question_is_a_button_not_a_wall() {
    // The regression this whole round is about: the SDK question is about
    // the *last* of twelve steps, and gating the sequence on it left the
    // panel unable to start at all --- a dimmed button that `Enter` could
    // not act on, before `Find Python 3.12` had even run.
    let (mut app, root) = install_app("sdk-gate");
    open(&mut app, &root.join("ws"));

    let installer = app.installer.as_ref().unwrap();
    assert!(installer.prereqs_ready(), "the fixtures satisfy these");
    assert!(
        installer.can_start(),
        "the sequence is startable — the SDK is not its gate"
    );
    assert_eq!(installer.action(), Action::PickToolchains);

    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("Pick SDK toolchains"),
        "the button says what pressing it does:\n{frame}"
    );
    assert!(frame.contains("no SDK toolchains picked"), "{frame}");

    // Enter acts. It used to do nothing at all.
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(app.overlay, Some(Overlay::SdkToolchains { .. })),
        "the button opens the picker: {:?}",
        app.overlay
    );
    app.handle(key(KeyCode::Char(' ')));
    app.handle(key(KeyCode::Enter));

    let installer = app.installer.as_ref().unwrap();
    assert_eq!(installer.action(), Action::Install);
    let frame = render(&mut app, 100, 40);
    assert!(frame.contains("ready · 1 toolchain"), "{frame}");

    // And now the first step actually runs --- the thing that was stuck.
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Char('y')));
    assert!(
        pump_until(&mut app, 30, |app| app
            .install_steps()
            .is_some_and(|steps| steps[0] != StepState::Pending)),
        "the sequence has to leave the starting line"
    );
}

#[test]
fn skipping_the_sdk_answers_its_question_too() {
    let (mut app, root) = install_app("sdk-skip-answers");
    open(&mut app, &root.join("ws"));
    assert_eq!(
        app.installer.as_ref().unwrap().action(),
        Action::PickToolchains
    );

    app.handle(key(KeyCode::Char('s')));
    let installer = app.installer.as_ref().unwrap();
    assert_eq!(installer.action(), Action::Install);
    assert!(installer.sdk_ready());
    let frame = render(&mut app, 100, 40);
    assert!(frame.contains("ready · SDK skipped"), "{frame}");
}

#[test]
fn every_button_state_either_acts_or_explains_itself() {
    // The property the split between `start_label` and the key handler
    // used to break: one decision, so a label can never name an action
    // that `Enter` does not have.
    for action in [
        Action::Stop,
        Action::Blocked,
        Action::PickToolchains,
        Action::Install,
        Action::Retry,
        Action::Adopt,
        Action::InstallSdk,
        Action::Done,
    ] {
        assert!(!action.label().is_empty());
        // Only the two whose explanation is elsewhere on screen are inert:
        // an open prerequisite (the checklist says so) and nothing left.
        assert_eq!(
            action.enabled(),
            !matches!(action, Action::Blocked | Action::Done)
        );
    }
}

#[test]
fn a_running_installation_is_not_left_by_reflex() {
    let (mut app, root) = install_app("no-escape");
    open(&mut app, &root.join("ws"));
    accept_confirm(&mut app);
    assert!(
        pump_until(&mut app, 30, |app| app
            .installer
            .as_ref()
            .is_some_and(chiptui::install::Installer::is_busy)),
        "a step must be running"
    );

    app.handle(key(KeyCode::Esc));
    assert!(
        matches!(app.overlay, Some(Overlay::ZephyrInstall)),
        "esc must not close the modal while a step runs — Stop is the way out"
    );
    app.handle(key(KeyCode::Char('q')));
    assert!(matches!(app.overlay, Some(Overlay::ZephyrInstall)));
}

#[test]
fn the_install_button_asks_where_and_the_picker_creates_the_workspace_inside() {
    let (mut app, root) = install_app("button");
    // The action stack's first row with nothing resolved.
    app.focus = chiptui::app::Focus::Build;
    app.build.as_mut().unwrap().cursor = 0;
    assert_eq!(
        app.build
            .as_ref()
            .unwrap()
            .action_at(&app.manager.capabilities(), 0),
        Some(chiptui::build::BuildAction::InstallZephyr)
    );
    app.handle(key(KeyCode::Enter));
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::DirPicker {
                purpose: chiptui::workspace::DirPurpose::Install,
                ..
            })
        ),
        "the button asks where before it asks whether"
    );

    // Accepting the folder opens the installer rooted one level in.
    let parent = root.join("ws");
    std::fs::create_dir_all(&parent).unwrap();
    app.open_installer(parent.clone());
    assert_eq!(app.installer.as_ref().unwrap().root, parent.join("zephyr"));
}

#[test]
fn pointing_at_a_half_built_workspace_resumes_it_rather_than_nesting_another() {
    let (mut app, root) = install_app("nesting");
    let half = root.join("zephyrproject");
    std::fs::create_dir_all(half.join(".west")).unwrap();

    app.open_installer(half.clone());
    assert_eq!(
        app.installer.as_ref().unwrap().root,
        half,
        "a folder that already carries .west/ is the workspace, not its parent"
    );
}

#[test]
fn the_modal_is_legible_at_the_declared_minimum() {
    let (mut app, root) = install_app("minimum");
    // A prerequisite that fails, so the row carries a version, a minimum
    // *and* a remedy --- the widest a prerequisite row ever gets.
    app.set_installer_tool_path("dtc", fake("dtc-old"));
    open(&mut app, &root.join("ws"));

    let frame = render(&mut app, 80, 32);
    // Every section reaches the screen at 80x32: the three headings, all
    // four prerequisites, the first and last step, and the footer.
    for needle in [
        "Prerequisites",
        "Steps",
        "Output",
        "cmake",
        "dtc",
        "pyenv",
        "python",
        "Find Python 3.12",
        "Install the SDK",
        "▶  Install",
        "prerequisites missing",
    ] {
        assert!(
            frame.contains(needle),
            "`{needle}` is clipped at the declared minimum:\n{frame}"
        );
    }
    // Columns line up: the minimum starts at the same offset on every row
    // that states one, whatever the version before it was.
    let column = |needle: &str| {
        frame
            .lines()
            .find(|line| line.contains(needle))
            .and_then(|line| line.find("min "))
    };
    assert_eq!(column("✓ cmake"), column("✗ dtc"));
    assert!(column("✓ cmake").is_some());
}

// ---------------------------------------------------------------------------
// A *second* installation: a machine that already has one, pointed at another
// folder. The environment questions are always re-answerable --- `Zephyr path`
// is how an answer is changed --- so the installer has to be reachable from
// there, and finishing has to switch the active installation rather than
// quietly write beside it.
// ---------------------------------------------------------------------------

/// The `Zephyr path` row: the shortcuts overlay's `e` letter (`ctrl+k`)
/// enters the Project pane's checklist with the cursor on the first open
/// question, and this row is its first.
fn press_zephyr_path_row(app: &mut App) {
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('k'),
        KeyModifiers::CONTROL,
    )));
    app.handle(key(KeyCode::Char('e')));
    app.handle(key(KeyCode::Home));
    app.handle(key(KeyCode::Enter));
}

#[test]
fn a_refused_folder_offers_the_installer_instead_of_only_saying_no() {
    let (mut app, root) = install_app("offer");
    let empty = root.join("elsewhere");
    std::fs::create_dir_all(&empty).unwrap();

    app.overlay = Some(Overlay::DirPicker {
        purpose: chiptui::workspace::DirPurpose::Installation,
        path: empty.clone(),
        selected: 0,
        error: None,
    });
    app.handle(key(KeyCode::Enter));

    let Some(Overlay::ConfirmInstallHere { dir, reason, .. }) = app.overlay.clone() else {
        panic!(
            "a refused folder must offer a way forward: {:?}",
            app.overlay
        );
    };
    assert_eq!(dir, empty);
    assert!(
        reason.contains(".west") && reason.contains("docs.zephyrproject.org"),
        "the offer still carries the refusal it answers: {reason}"
    );

    // Declining puts the picker back exactly where the refusal left it ---
    // the overlay slot is one deep, so the offer has to restore it.
    app.handle(key(KeyCode::Char('n')));
    let Some(Overlay::DirPicker { path, error, .. }) = app.overlay.clone() else {
        panic!("declining must return to the picker");
    };
    assert_eq!(path, empty);
    assert!(error.is_some_and(|error| error.contains(".west")));

    // Accepting opens the installer one level in.
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Char('y')));
    assert!(matches!(app.overlay, Some(Overlay::ZephyrInstall)));
    assert_eq!(app.installer.as_ref().unwrap().root, empty.join("zephyr"));
}

#[test]
fn the_offer_names_what_is_actually_in_the_folder() {
    let (mut app, root) = install_app("wording");
    let offer_title = |app: &mut App, dir: &Path| {
        app.overlay = Some(Overlay::DirPicker {
            purpose: chiptui::workspace::DirPurpose::Installation,
            path: dir.to_path_buf(),
            selected: 0,
            error: None,
        });
        app.handle(key(KeyCode::Enter));
        assert!(
            matches!(app.overlay, Some(Overlay::ConfirmInstallHere { .. })),
            "every refusal offers: {:?}",
            app.overlay
        );
        render(app, 100, 40)
    };

    // Nothing there at all.
    let empty = root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let frame = offer_title(&mut app, &empty);
    assert!(frame.contains("Install Zephyr in here?"), "{frame}");

    // `west init` ran and stopped: resumable, not a fresh install.
    let partial = root.join("partial");
    std::fs::create_dir_all(partial.join(".west")).unwrap();
    let frame = offer_title(&mut app, &partial);
    assert!(
        frame.contains("Finish the installation in here?"),
        "{frame}"
    );

    // A complete installation sitting in the folder's `zephyr/` --- the
    // case the picker cannot accept on its own, because it validates the
    // directory it was given and not its children.
    let nested = root.join("nested");
    installed_workspace(&nested.join("zephyr"));
    let frame = offer_title(&mut app, &nested);
    assert!(frame.contains("Use the installation in here?"), "{frame}");
}

#[test]
fn an_installation_already_there_is_adopted_and_never_reinstalled() {
    let (mut app, root) = install_app("adopt");
    let parent = root.join("nested");
    let ws = installed_workspace(&parent.join("zephyr"));
    // A *finished* installation: workspace and SDK both. Without the
    // bundle the panel would offer to install it instead --- which is its
    // own test below.
    installed_sdk(&ws);
    // A prerequisite below the minimum: adopting runs nothing, so it must
    // not be gated on the tools that would *build* Zephyr.
    app.set_installer_tool_path("cmake", fake("cmake-old"));

    open(&mut app, &parent);
    let installer = app.installer.as_ref().unwrap();
    assert_eq!(installer.root, ws);
    assert!(installer.adopted(), "a complete workspace is adopted");
    assert!(
        !installer.prereqs_ready(),
        "the fixture is deliberately blocked, to prove adopting ignores it"
    );

    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("Use this installation"),
        "the button offers to adopt:\n{frame}"
    );
    assert!(frame.contains("already installed"), "{frame}");

    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.processes.running_count(),
        0,
        "adopting must spawn nothing"
    );
    assert!(app.installer.is_none());
    assert!(
        user_config(&root).contains(&format!("workspace = \"{}\"", ws.display())),
        "the adopted installation is recorded like any other pick"
    );
    assert_eq!(app.workspace.as_ref().unwrap().dir(), Some(&ws));
}

#[test]
fn a_second_installation_switches_the_active_one_and_says_so() {
    // A fully answered environment before the app ever starts: an
    // installation *and* a projects folder, so `bootstrap` resolves them
    // the way a real session would.
    let (mut app, root) = install_app_configured("second", |root| {
        installed_workspace(&root.join("first"));
        std::fs::create_dir_all(root.join("apps")).unwrap();
        format!(
            "[zephyr]\nworkspace = \"{}\"\nprojects = \"{}\"\n",
            root.join("first").display(),
            root.join("apps").display()
        )
    });
    let first = root.join("first");
    let apps = root.join("apps");
    assert_eq!(app.workspace.as_ref().unwrap().dir(), Some(&first));

    // The `Zephyr path` row is the door: it is how an answer is changed.
    press_zephyr_path_row(&mut app);
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::DirPicker {
                purpose: chiptui::workspace::DirPurpose::Installation,
                ..
            })
        ),
        "an answered row still opens its picker: {:?}",
        app.overlay
    );

    let second = root.join("second");
    std::fs::create_dir_all(&second).unwrap();
    app.overlay = Some(Overlay::DirPicker {
        purpose: chiptui::workspace::DirPurpose::Installation,
        path: second.clone(),
        selected: 0,
        error: None,
    });
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Char('y')));
    assert!(pump_until(&mut app, 10, probes_done));

    accept_confirm(&mut app);
    assert!(
        pump_until(&mut app, 60, |app| app.installer.is_none()),
        "the second installation must run to the end"
    );

    let target = second.join("zephyr");
    let config = user_config(&root);
    assert!(
        config.contains(&format!("workspace = \"{}\"", target.display())),
        "the new installation becomes the active one:\n{config}"
    );
    assert!(
        !config.contains(&format!("workspace = \"{}\"", first.display())),
        "the old line is replaced, not duplicated:\n{config}"
    );
    assert!(
        config.contains(&format!("projects = \"{}\"", apps.display())),
        "the projects answer survives the switch:\n{config}"
    );
    assert_eq!(app.workspace.as_ref().unwrap().dir(), Some(&target));

    // The switch is named, not left to be discovered in the pane.
    assert!(
        log_mentions(&app, "switched from"),
        "the log must name the installation it replaced"
    );
    // And an answered question is not asked again.
    assert!(
        app.overlay.is_none(),
        "the projects folder is already configured: {:?}",
        app.overlay
    );
}

#[test]
fn the_first_installation_still_chains_into_the_projects_question() {
    // The regression guard for the condition above: with no projects
    // folder configured, finishing must still ask for one.
    let (mut app, root) = install_app("chain");
    open(&mut app, &root.join("ws"));
    accept_confirm(&mut app);
    assert!(pump_until(&mut app, 60, |app| app.installer.is_none()));
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::DirPicker {
                purpose: chiptui::workspace::DirPurpose::Projects,
                ..
            })
        ),
        "an unanswered projects folder is still the next question: {:?}",
        app.overlay
    );
}

// ---------------------------------------------------------------------------
// What a failing step must not cost. `west sdk list` reads the CMake user
// package registry and dies on a fresh machine by design, which is how this
// whole area got found: a step that fails used to take the entire
// installation down with it, unwritten.
// ---------------------------------------------------------------------------

/// Seeds `<root>` as a workspace whose venv is already built, with a `west`
/// that delegates to the fixture except for the `sdk` subcommand named in
/// `break_on` (`"install"`, `"list"`, `"sdk"` for both, or a name no
/// subcommand has, for a west that works).
///
/// Pre-seeding rather than overriding: the venv's binaries are absolute
/// paths the installer *derives* from the root, so this is the only seam
/// that reaches them --- and it doubles as the resumption path, since
/// `Step::already_done` reads exactly these files.
fn workspace_with_broken_west(root: &Path, break_on: &str) {
    std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
    std::fs::write(root.join(".python-version"), "3.12.13\n").unwrap();
    let shim = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"sdk\" ] && {{ [ \"{break_on}\" = \"sdk\" ] || [ \"$2\" = \"{break_on}\" ]; }}; then\n\
         \x20   printf 'FATAL ERROR: broken sdk {break_on}\\n' >&2\n\
         \x20   exit 1\n\
         fi\n\
         exec \"{west}\" \"$@\"\n",
        west = fake("west"),
    );
    let path = root.join(".venv/bin/west");
    std::fs::write(&path, shim).unwrap();
    std::fs::copy(fake("pip"), root.join(".venv/bin/pip")).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

#[test]
fn the_sdk_confirmation_failing_does_not_undo_the_install() {
    let (mut app, root) = install_app("sdk-list-fails");
    let target = root.join("ws/zephyr");
    workspace_with_broken_west(&target, "list");

    open(&mut app, &root.join("ws"));
    accept_confirm(&mut app);
    assert!(
        pump_until(&mut app, 60, |app| app.installer.is_none()),
        "an optional step's failure must not stop the sequence"
    );

    // The SDK really was installed; only the confirmation of it failed.
    assert!(target.join("zephyr-sdk-0.17.0/sdk_version").is_file());
    let config = user_config(&root);
    assert!(
        config.contains(&format!("workspace = \"{}\"", target.display())),
        "the installation is recorded despite the failed confirmation:\n{config}"
    );
    assert!(
        config.contains("zephyr-sdk-0.17.0"),
        "and so is the bundle it installed:\n{config}"
    );
}

#[test]
fn a_fatal_failure_still_records_a_workspace_that_is_already_usable() {
    let (mut app, root) = install_app("salvage");
    let target = root.join("ws/zephyr");
    workspace_with_broken_west(&target, "install");

    open(&mut app, &root.join("ws"));
    accept_confirm(&mut app);
    assert!(
        pump_until(&mut app, 60, |app| app
            .installer
            .as_ref()
            .is_some_and(chiptui::install::Installer::stopped)),
        "the SDK install failing is fatal — it is not an optional step"
    );

    // `west init` and `west update` both ran, so what is on disk is a real
    // installation. Losing it because a later step failed was the bug.
    assert!(target.join(".west/config").is_file());
    assert!(target.join("zephyr/VERSION").is_file());
    let config = user_config(&root);
    assert!(
        config.contains(&format!("workspace = \"{}\"", target.display())),
        "a usable workspace is recorded even though the run stopped:\n{config}"
    );
    assert_eq!(app.workspace.as_ref().unwrap().dir(), Some(&target));
    assert!(
        log_mentions(&app, "is usable"),
        "the log must say why it was recorded anyway"
    );

    // And the run is still there to resume: modal open, panel alive.
    assert!(matches!(app.overlay, Some(Overlay::ZephyrInstall)));
    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("Retry"),
        "the failed step is retryable:\n{frame}"
    );
}

#[test]
fn an_installation_missing_only_its_sdk_can_finish_it() {
    // Skipping the SDK (or having its step fail) used to be a dead end:
    // reopening the installer found a `Complete` workspace, offered only
    // `Use this installation`, and left no way to run the one step that
    // was missing.
    let (mut app, root) = install_app("adopt-no-sdk");
    let parent = root.join("nested");
    let ws = installed_workspace(&parent.join("zephyr"));
    // The venv west the SDK step will actually invoke.
    workspace_with_broken_west(&ws, "__none__");

    open(&mut app, &parent);
    let installer = app.installer.as_ref().unwrap();
    assert_eq!(installer.root, ws);
    assert!(installer.adopted());
    assert!(installer.sdk_missing());
    // The toolchain question comes first here too --- same rule, whichever
    // door reached the SDK step.
    assert_eq!(installer.action(), Action::PickToolchains);

    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::SdkToolchains { .. })));
    app.handle(key(KeyCode::Char(' ')));
    app.handle(key(KeyCode::Enter));

    let installer = app.installer.as_ref().unwrap();
    assert_eq!(installer.action(), Action::InstallSdk);
    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("Install the SDK"),
        "the button offers to finish it:\n{frame}"
    );
    assert!(frame.contains("installed, no SDK"), "{frame}");

    app.handle(key(KeyCode::Enter));
    // The adoption is recorded before anything runs: closing the modal
    // mid-run must not lose an answer that was already correct.
    assert!(
        user_config(&root).contains(&format!("workspace = \"{}\"", ws.display())),
        "the installation is recorded up front"
    );

    assert!(
        pump_until(&mut app, 60, |app| app.installer.is_none()),
        "the SDK step must run to the end"
    );
    assert!(ws.join("zephyr-sdk-0.17.0/sdk_version").is_file());

    // And *only* the SDK ran: `west init` on an existing tree would have
    // been the bug, since `next_step()` answers 0 on an installed one.
    assert!(
        !log_mentions(&app, "west init"),
        "an installed workspace must not be re-initialised"
    );
    let config = user_config(&root);
    assert!(
        config.contains("zephyr-sdk-0.17.0"),
        "and the bundle it just installed is recorded:\n{config}"
    );
}

#[test]
fn a_bundle_of_the_wrong_version_does_not_answer_the_sdk_step() {
    // The workspace pins its SDK version; a bundle of another version is
    // somebody else's SDK, and building against it would be the silent
    // wrong answer.
    let (mut app, root) = install_app("sdk-version");
    let parent = root.join("nested");
    let ws = installed_workspace(&parent.join("zephyr")); // pins 0.17.0
    std::fs::create_dir_all(ws.join("zephyr-sdk-0.16.0")).unwrap();

    open(&mut app, &parent);
    let installer = app.installer.as_ref().unwrap();
    assert!(installer.adopted(), "the workspace itself is complete");
    assert!(
        installer.sdk_missing(),
        "0.16.0 is not the 0.17.0 this workspace asks for"
    );
    assert_eq!(installer.action(), Action::PickToolchains);

    // The matching bundle does answer it.
    installed_sdk(&ws);
    let mut app2 = install_app("sdk-version-ok").0;
    for tool in ["cmake", "dtc", "pyenv", "python3"] {
        app2.set_installer_tool_path(tool, fake(tool));
    }
    app2.open_installer(parent.clone());
    let installer = app2.installer.as_ref().unwrap();
    assert!(!installer.sdk_missing());
    assert_eq!(installer.action(), Action::Adopt);
}

// ---------------------------------------------------------------------------
// Adding a toolchain to an SDK that is already installed. West finds the
// version registered, skips the download entirely, and runs `setup.sh -t`
// per name it was given --- so this costs one toolchain, not the bundle.
// ---------------------------------------------------------------------------

#[test]
fn adding_a_toolchain_asks_west_only_for_the_missing_one() {
    let (mut app, root) = install_app("add-toolchain");
    let parent = root.join("nested");
    let ws = installed_workspace(&parent.join("zephyr"));
    installed_sdk_with(&ws, &["arm-zephyr-eabi"]);
    workspace_with_broken_west(&ws, "__none__");

    open(&mut app, &parent);
    let installer = app.installer.as_ref().unwrap();
    assert!(installer.adopted());
    assert!(
        !installer.sdk_missing(),
        "the bundle is there --- what is missing is a toolchain inside it"
    );
    assert_eq!(
        installer.installed_toolchains(),
        vec!["arm-zephyr-eabi".to_string()]
    );
    // Nothing picked yet: nothing to add, so this is still a plain adopt.
    assert_eq!(installer.action(), Action::Adopt);

    // Pick the one already there *and* one that is not.
    app.handle(key(KeyCode::Char('t')));
    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("arm-zephyr-eabi"),
        "the picker lists it:\n{frame}"
    );
    pick_toolchain(&mut app, "arm-zephyr-eabi");
    pick_toolchain(&mut app, "riscv64-zephyr-elf");
    app.handle(key(KeyCode::Enter));

    let installer = app.installer.as_ref().unwrap();
    assert_eq!(installer.picked_toolchains.len(), 2);
    // Only the absent one is pending --- the decision this test locks.
    assert_eq!(
        installer.pending_toolchains(),
        vec!["riscv64-zephyr-elf".to_string()]
    );
    assert_eq!(installer.action(), Action::AddToolchains);

    let sdk_index = Step::ALL
        .iter()
        .position(|step| *step == Step::SdkInstall)
        .unwrap();
    let rendered = installer.step_command(sdk_index).unwrap().to_string();
    assert!(
        rendered.ends_with("-t riscv64-zephyr-elf"),
        "the command must carry only what is missing: {rendered}"
    );
    assert!(
        !rendered.contains("arm-zephyr-eabi"),
        "re-asking for an installed toolchain is the thing to avoid: {rendered}"
    );

    // The checklist has to agree with the button beside it.
    assert_eq!(
        app.install_steps().unwrap()[sdk_index],
        StepState::Pending,
        "a pick the bundle lacks makes the SDK step something to run again"
    );

    let frame = render(&mut app, 100, 40);
    assert!(
        frame.contains("Add SDK toolchains"),
        "the button offers exactly that:\n{frame}"
    );
    assert!(frame.contains("installed · 1 to add"), "{frame}");

    // Both rows carry a ✓, so only the colour separates "already here"
    // from "about to be installed" --- a text assertion cannot see the
    // difference this test is about.
    app.handle(key(KeyCode::Char('t')));
    let palette = app.theme_palette();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
    terminal
        .draw(|frame| chiptui::ui::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mark_colour = |needle: &str| {
        let row = (0..buffer.area.height).find(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, *y)].symbol().to_string())
                .collect::<String>()
                .contains(needle)
        })?;
        (0..buffer.area.width)
            .find(|x| buffer[(*x, row)].symbol() == "✓")
            .map(|x| buffer[(x, row)].fg)
    };
    assert_eq!(
        mark_colour("riscv64-zephyr-elf"),
        Some(palette.success),
        "a toolchain about to be installed is marked live"
    );
    assert_eq!(
        mark_colour("arm-zephyr-eabi"),
        Some(palette.muted),
        "one already unpacked in the SDK is marked, but muted"
    );
    app.handle(key(KeyCode::Enter));

    app.handle(key(KeyCode::Enter));
    assert!(
        pump_until(&mut app, 60, |app| app.installer.is_none()),
        "the SDK step must run to the end"
    );

    let gnu = ws.join("zephyr-sdk-0.17.0/gnu");
    assert!(
        gnu.join("arm-zephyr-eabi").is_dir(),
        "the toolchain that was already there must survive"
    );
    assert!(
        gnu.join("riscv64-zephyr-elf").is_dir(),
        "and the new one is unpacked beside it"
    );
}

#[test]
fn an_sdk_that_already_carries_the_pick_runs_nothing() {
    let (mut app, root) = install_app("add-nothing");
    let parent = root.join("nested");
    let ws = installed_workspace(&parent.join("zephyr"));
    installed_sdk_with(&ws, &["arm-zephyr-eabi"]);

    open(&mut app, &parent);
    app.handle(key(KeyCode::Char('t')));
    pick_toolchain(&mut app, "arm-zephyr-eabi");
    app.handle(key(KeyCode::Enter));

    let installer = app.installer.as_ref().unwrap();
    assert!(installer.pending_toolchains().is_empty());
    assert_eq!(
        installer.action(),
        Action::Adopt,
        "picking what is already installed adds nothing to do"
    );

    app.handle(key(KeyCode::Enter));
    assert_eq!(
        app.processes.running_count(),
        0,
        "adopting must still spawn nothing"
    );
}

/// Moves the toolchain picker's cursor onto `name` and toggles it, leaving
/// the cursor back at the top so calls compose.
fn pick_toolchain(app: &mut App, name: &str) {
    let index = position_of(name);
    for _ in 0..index {
        app.handle(key(KeyCode::Down));
    }
    app.handle(key(KeyCode::Char(' ')));
    for _ in 0..index {
        app.handle(key(KeyCode::Up));
    }
}

#[test]
fn the_dashboard_s_key_opens_the_sdk_toolchains_directly() {
    // The errand this shortcut exists for: an SDK that is fine except for
    // one target a new board needs. Reaching the picker used to mean
    // re-answering the path question the config already holds.
    let (mut app, root) = install_app_configured("s-key", |root| {
        let ws = installed_workspace(&root.join("zephyrproject"));
        installed_sdk_with(&ws, &["arm-zephyr-eabi"]);
        format!(
            "[zephyr]\nworkspace = \"{}\"\nprojects = \"{}\"\n",
            ws.display(),
            root.join("apps").display()
        )
    });
    std::fs::create_dir_all(root.join("apps")).unwrap();
    let ws = root.join("zephyrproject");
    assert_eq!(app.workspace.as_ref().unwrap().dir(), Some(&ws));
    assert!(app.overlay.is_none());

    app.handle(key(KeyCode::Char('s')));
    assert!(
        matches!(app.overlay, Some(Overlay::SdkToolchains { .. })),
        "`s` lands straight on the picker: {:?}",
        app.overlay
    );
    // Rooted at the configured workspace, not at a `zephyr/` inside it.
    let installer = app.installer.as_ref().unwrap();
    assert_eq!(installer.root, ws);
    assert!(installer.adopted());
    assert_eq!(
        installer.installed_toolchains(),
        vec!["arm-zephyr-eabi".to_string()]
    );

    // And the picker leads where it should: pick, leave, add.
    pick_toolchain(&mut app, "riscv64-zephyr-elf");
    app.handle(key(KeyCode::Enter));
    assert!(matches!(app.overlay, Some(Overlay::ZephyrInstall)));
    assert_eq!(
        app.installer.as_ref().unwrap().action(),
        Action::AddToolchains
    );
}

#[test]
fn the_s_key_invents_nothing_without_an_installation() {
    let (mut app, _root) = install_app("s-key-unconfigured");
    app.handle(key(KeyCode::Char('s')));
    assert!(
        app.overlay.is_none(),
        "no installation means nothing to add to: {:?}",
        app.overlay
    );
    assert!(app.installer.is_none(), "and no panel is created");
    assert!(
        log_mentions(&app, "Zephyr path"),
        "the log points at the question that is actually open"
    );
}
