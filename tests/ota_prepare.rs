//! Preparing a Zephyr project for OTA, end to end: the requirement probe
//! through a fixture `smpmgr`, the five writes, what each file holds
//! afterwards, and that a re-run is a no-op.
//!
//! Driven the way `tests/zephyr_install.rs` drives the installer, except
//! that the panel is not wired into `App` yet (that is M5's modal), so the
//! tests hold the `Prepare` and its `ProcessManager` directly.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chiptui::ota::prepare::{Prepare, Requirement, SlotCheck, Step, ToolProbe};
use chiptui::ota::{OtaConfig, Transport};
use chiptui::process::ProcessManager;
use chiptui::stepper::{Phase, StepState};

mod common;
use common::fake;

const BOARD: &str = "xiao_esp32c3";

fn temp_project(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("chiptui-ota-prepare-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config() -> OtaConfig {
    OtaConfig {
        address: Some("192.168.1.42".to_string()),
        ..OtaConfig::default()
    }
}

/// A panel over a fresh project with the probe pointed at the fixture.
fn prepare(root: &Path, tag: &str) -> (Prepare, ProcessManager) {
    let mut panel = Prepare::new(root, None, BOARD, None, config());
    panel.set_tool(fake(tag));
    (panel, ProcessManager::new())
}

/// Drains process events until the requirement probes have all answered.
fn settle_probes(panel: &mut Prepare, processes: &mut ProcessManager) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !panel
        .requirements
        .iter()
        .all(|state| state.probe != ToolProbe::Probing)
        && Instant::now() < deadline
    {
        for event in processes.drain() {
            panel.on_process(&event);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn read(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative)).unwrap()
}

/// A `zephyr.dts` carrying the three nodes an A/B layout needs, in the
/// shape `devicetree::parse` reads (the path annotations are load-bearing:
/// only a node under `/partitions/` counts).
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

fn built_with_slots(root: &Path, dts: &str) {
    let dir = root.join("build/zephyr");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("zephyr.dts"), dts).unwrap();
}

/// The root-as-project layout: a repository whose root is an out-of-tree
/// board module and whose application sits in `app/`. Three of the five
/// writes are the *application's* --- Zephyr reads `sysbuild.conf`,
/// `VERSION` and the `boards/` fragment out of the source directory
/// `west build` is pointed at, so a copy at the repository root above it
/// is a file nothing ever opens. `chiptui.toml` stays at the root, which
/// is the project's own file and the only one there is.
#[test]
fn a_repository_project_writes_the_application_files_beside_the_application() {
    let root = temp_project("app-root");
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let mut panel = Prepare::new(&root, Some(app.clone()), BOARD, None, config());
    panel.set_tool(fake("smpmgr"));
    let mut processes = ProcessManager::new();
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    assert!(
        panel.start().finished,
        "every step settles: {:?}",
        panel.steps
    );

    for name in ["sysbuild.conf", "VERSION", "boards/xiao_esp32c3.conf"] {
        assert!(
            app.join(name).is_file(),
            "{name} belongs to the application"
        );
        assert!(
            !root.join(name).exists(),
            "{name} must not be written where Zephyr never looks"
        );
    }
    assert!(
        root.join("chiptui.toml").is_file() && !app.join("chiptui.toml").exists(),
        "the project's own file stays at the root"
    );

    // And a re-run reads its own answers back from where it put them.
    let mut again = Prepare::new(&root, Some(app), BOARD, None, config());
    again.set_tool(fake("smpmgr"));
    assert!(
        again
            .steps
            .iter()
            .all(|state| *state == StepState::Done || *state == StepState::Skipped),
        "an instrumented project opens with nothing left to do: {:?}",
        again.steps
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_plain_project_is_instrumented_end_to_end() {
    let root = temp_project("plain");
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);

    assert!(panel.requirements_ready(), "the fixture answers --version");
    assert!(panel.can_start());
    let update = panel.start();
    assert!(update.finished, "every step settles: {:?}", panel.steps);
    assert_eq!(panel.phase, Phase::Finished);

    // sysbuild.conf: the two sysbuild symbols inside a managed block.
    let sysbuild = read(&root, "sysbuild.conf");
    assert!(sysbuild.contains("# >>> chiptui:ota"));
    assert!(sysbuild.contains("SB_CONFIG_BOOTLOADER_MCUBOOT=y"));
    assert!(sysbuild.contains("SB_CONFIG_MCUBOOT_MODE_SWAP_USING_MOVE=y"));

    // VERSION: a parseable 0.1.0.
    let version = read(&root, "VERSION");
    assert!(version.contains("VERSION_MAJOR = 0"));
    assert!(version.contains("VERSION_MINOR = 1"));
    assert!(version.contains("PATCHLEVEL = 0"));
    assert!(version.contains("VERSION_TWEAK = 0"));

    // The board fragment: core + UDP transport, the situational tweaks
    // present but commented, and no net shell (default off).
    let fragment = read(&root, "boards/xiao_esp32c3.conf");
    assert!(fragment.contains("# >>> chiptui:ota"));
    assert!(fragment.contains("CONFIG_BOOTLOADER_MCUBOOT=y"));
    // The flash map the image manager's mcuboot implementation depends on.
    // Without it nothing selects MCUBOOT_BOOTUTIL_LIB while
    // `subsys/dfu/img_util` links `-lMCUBOOT_BOOTUTIL` regardless, so a
    // real board configures, compiles everything and dies at the link.
    assert!(fragment.contains("CONFIG_IMG_MANAGER=y"));
    assert!(fragment.contains("CONFIG_FLASH=y"));
    assert!(fragment.contains("CONFIG_FLASH_MAP=y"));
    assert!(fragment.contains("CONFIG_MCUMGR_TRANSPORT_UDP=y"));
    assert!(fragment.contains("CONFIG_MCUMGR_TRANSPORT_UDP_IPV4=y"));
    assert!(fragment.contains("# CONFIG_NET_MAX_CONN=8"));
    assert!(!fragment.lines().any(|line| line == "CONFIG_NET_MAX_CONN=8"));
    assert!(!fragment.lines().any(|line| line == "CONFIG_NET_SHELL=y"));
    assert!(!fragment.contains("ota-netshell"));
    // The address log is the other opt-in, off by the same default.
    assert!(!fragment.contains("ota-address-log"));

    // chiptui.toml: the [ota] answers recorded.
    let toml = read(&root, "chiptui.toml");
    assert!(toml.contains("[ota]"));
    assert!(toml.contains("method = \"mcumgr\""));
    assert!(toml.contains("transport = \"udp\""));
    assert!(toml.contains("address = \"192.168.1.42\""));

    // Every required step Done, the opt-in one Skipped.
    for (index, step) in Step::ALL.iter().enumerate() {
        let expected = if step.optional() {
            StepState::Skipped
        } else {
            StepState::Done
        };
        assert_eq!(panel.steps[index], expected, "{step:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_rerun_is_a_noop_with_every_step_done() {
    let root = temp_project("rerun");
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    assert!(panel.start().finished);

    let before: Vec<String> = [
        "sysbuild.conf",
        "VERSION",
        "boards/xiao_esp32c3.conf",
        "chiptui.toml",
    ]
    .iter()
    .map(|path| read(&root, path))
    .collect();

    // A fresh panel over the prepared tree reads every step off the
    // filesystem and finds nothing to do --- even with the requirement
    // answered, `start` changes nothing.
    let (mut again, _) = prepare(&root, "smpmgr");
    assert!(again.next_step().is_none());
    for (index, step) in Step::ALL.iter().enumerate() {
        let expected = if step.optional() {
            StepState::Skipped
        } else {
            StepState::Done
        };
        assert_eq!(again.steps[index], expected, "{step:?}");
    }
    again.requirements.iter_mut().for_each(|state| {
        state.probe = ToolProbe::Present("0.19.0".to_string());
    });
    assert!(!again.can_start(), "nothing left to run");
    let update = again.start();
    assert!(
        !update.finished && !update.stopped,
        "a no-op run changes nothing"
    );

    let after: Vec<String> = [
        "sysbuild.conf",
        "VERSION",
        "boards/xiao_esp32c3.conf",
        "chiptui.toml",
    ]
    .iter()
    .map(|path| read(&root, path))
    .collect();
    assert_eq!(before, after, "not one byte moved");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_slot_precondition_blocks_only_when_the_build_says_no() {
    let root = temp_project("slots");

    // Built, and the layout has no slot1: the prepare refuses to start.
    built_with_slots(
        &root,
        &DTS_WITH_SLOTS.replace("slot1_partition", "storage_partition"),
    );
    let (mut panel, mut processes) = {
        let mut panel = Prepare::new(&root, None, BOARD, Some("build".to_string()), config());
        panel.set_tool(fake("smpmgr"));
        (panel, ProcessManager::new())
    };
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    match &panel.slots {
        SlotCheck::Missing(missing, _) => assert_eq!(missing, &vec!["slot1_partition"]),
        other => panic!("expected Missing, got {other:?}"),
    }
    assert!(!panel.can_start(), "a board without two slots blocks");
    assert!(
        !root.join("sysbuild.conf").exists(),
        "and nothing was written"
    );

    // The same project with a real layout prepares fine.
    built_with_slots(&root, DTS_WITH_SLOTS);
    let mut panel = Prepare::new(&root, None, BOARD, Some("build".to_string()), config());
    panel.requirements.iter_mut().for_each(|state| {
        state.probe = ToolProbe::Present("0.19.0".to_string());
    });
    assert!(matches!(panel.slots, SlotCheck::Found(_)));
    assert!(panel.can_start());
    assert!(panel.start().finished);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_netshell_toggle_writes_its_own_block_in_the_same_file() {
    let root = temp_project("netshell");
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    panel.toggle_netshell();
    assert!(panel.start().finished);

    let fragment = read(&root, "boards/xiao_esp32c3.conf");
    assert!(fragment.contains("# >>> chiptui:ota-netshell"));
    assert!(fragment.contains("CONFIG_NET_SHELL=y"));
    assert!(
        fragment.contains("# >>> chiptui:ota ---"),
        "both blocks share the file"
    );

    // A fresh panel reads the opt-in back as done, not as skipped --- and
    // reads the *answer* back too, so the toggle knows which way it moves.
    let (again, _) = prepare(&root, "smpmgr");
    let index = Step::ALL
        .iter()
        .position(|step| *step == Step::NetShell)
        .unwrap();
    assert_eq!(again.steps[index], StepState::Done);
    assert!(
        again.netshell(),
        "the opt-in comes off the filesystem, like every other step's state"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Opting back out removes the block, and leaves every byte outside its
/// markers exactly as it was.
///
/// The toggle used to give up whenever the step read `Done` --- which is
/// the state a project that already has the block is in --- so `s` did
/// nothing, the heading kept offering to *add* what was already there, and
/// there was no way out of the block through the UI at all.
#[test]
fn the_netshell_toggle_removes_the_block_it_wrote() {
    let root = temp_project("netshell-off");
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    panel.toggle_netshell();
    assert!(panel.start().finished);
    let with_block = read(&root, "boards/xiao_esp32c3.conf");
    assert!(with_block.contains("CONFIG_NET_SHELL=y"));

    // A fresh panel over the prepared project, then opt back out.
    let (mut again, mut processes) = prepare(&root, "smpmgr");
    again.probe_requirements(&mut processes);
    settle_probes(&mut again, &mut processes);
    assert!(again.netshell());
    again.toggle_netshell();
    assert!(!again.netshell());

    let index = Step::ALL
        .iter()
        .position(|step| *step == Step::NetShell)
        .unwrap();
    assert_eq!(
        again.steps[index],
        StepState::Pending,
        "opting out with a block on disk is work, not a completion"
    );
    assert!(again.start().finished, "{:?}", again.steps);
    assert_eq!(
        again.steps[index],
        StepState::Skipped,
        "and once removed there is nothing there --- skipped, never done"
    );

    let without = read(&root, "boards/xiao_esp32c3.conf");
    assert!(!without.contains("CONFIG_NET_SHELL=y"));
    assert!(!without.contains("chiptui:ota-netshell"));
    // The other block, and everything around it, is untouched.
    assert!(without.contains("# >>> chiptui:ota ---"));
    assert!(without.contains("CONFIG_MCUMGR=y"));
    // And the round trip is stable: adding it back reproduces the file the
    // first write produced, byte for byte.
    let (mut back, mut processes) = prepare(&root, "smpmgr");
    back.probe_requirements(&mut processes);
    settle_probes(&mut back, &mut processes);
    back.toggle_netshell();
    assert!(back.start().finished);
    assert_eq!(
        read(&root, "boards/xiao_esp32c3.conf"),
        with_block,
        "add/remove/add leaves no accumulated blank lines"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_existing_sysbuild_conf_is_extended_never_replaced() {
    let root = temp_project("extend");
    std::fs::write(
        root.join("sysbuild.conf"),
        "# the project's own sysbuild configuration\nSB_CONFIG_EXTRA_WARNINGS=y\n",
    )
    .unwrap();
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    assert!(panel.start().finished);

    let sysbuild = read(&root, "sysbuild.conf");
    assert!(
        sysbuild.starts_with(
            "# the project's own sysbuild configuration\nSB_CONFIG_EXTRA_WARNINGS=y\n"
        )
    );
    assert!(sysbuild.contains("SB_CONFIG_BOOTLOADER_MCUBOOT=y"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_existing_board_fragment_is_extended_in_place() {
    let root = temp_project("fragment");
    std::fs::create_dir_all(root.join("boards")).unwrap();
    std::fs::write(root.join("boards/xiao_esp32c3.conf"), "CONFIG_WIFI=y\n").unwrap();
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    assert!(panel.start().finished);

    let fragment = read(&root, "boards/xiao_esp32c3.conf");
    assert!(fragment.starts_with("CONFIG_WIFI=y\n"));
    assert!(fragment.contains("CONFIG_BOOTLOADER_MCUBOOT=y"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_version_file_that_does_not_parse_stops_the_run_by_name() {
    let root = temp_project("version");
    std::fs::write(root.join("VERSION"), "# we track versions elsewhere\n").unwrap();
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    let update = panel.start();

    assert!(update.stopped);
    assert!(!update.finished);
    let reason = panel.stop_reason();
    assert!(
        reason.contains("VERSION"),
        "the refusal names the file: {reason}"
    );
    assert!(reason.contains("refusing to overwrite"), "{reason}");
    // Sysbuild ran before it; the board fragment and the config record did
    // not --- a failed step stops the sequence there.
    assert!(root.join("sysbuild.conf").exists());
    assert!(!root.join("boards/xiao_esp32c3.conf").exists());
    assert!(!root.join("chiptui.toml").exists());

    // A retry after the user fixes the file resumes rather than restarting.
    std::fs::write(
        root.join("VERSION"),
        "VERSION_MAJOR = 1\nVERSION_MINOR = 0\nPATCHLEVEL = 0\nVERSION_TWEAK = 0\n",
    )
    .unwrap();
    let mut panel = Prepare::new(&root, None, BOARD, None, config());
    panel.requirements.iter_mut().for_each(|state| {
        state.probe = ToolProbe::Present("0.19.0".to_string());
    });
    assert_eq!(
        panel.steps[0],
        StepState::Done,
        "sysbuild survived the retry"
    );
    assert!(panel.start().finished);
    assert_eq!(
        read(&root, "VERSION"),
        "VERSION_MAJOR = 1\nVERSION_MINOR = 0\nPATCHLEVEL = 0\nVERSION_TWEAK = 0\n",
        "the user's own version is kept"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_smpmgr_blocks_the_prepare_with_its_hint() {
    let root = temp_project("no-smpmgr");
    let (mut panel, mut processes) = prepare(&root, "smpmgr-does-not-exist");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);

    assert!(!panel.requirements_ready());
    assert!(!panel.can_start(), "the probe's answer gates the sequence");
    assert_eq!(Requirement::Smpmgr.install_hint(), "pipx install smpmgr");
    assert!(!panel.start().finished, "a blocked panel writes nothing");
    assert!(!root.join("sysbuild.conf").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_config_record_preserves_what_the_file_already_says() {
    let root = temp_project("config");
    std::fs::write(
        root.join("chiptui.toml"),
        "project_type = \"zephyr\"\n\n[[variant]]\nname = \"hardware\"\nboard = \"xiao_esp32c3\"\n",
    )
    .unwrap();
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    assert!(panel.start().finished);

    let toml = read(&root, "chiptui.toml");
    assert!(toml.contains("project_type = \"zephyr\""));
    assert!(toml.contains("[[variant]]"));
    assert!(toml.contains("[ota]"));
    assert!(toml.contains("transport = \"udp\""));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_serial_transport_prepares_with_the_uart_symbol() {
    let root = temp_project("serial");
    let target = OtaConfig {
        transport: Transport::Serial,
        address: Some("/dev/ttyACM0".to_string()),
        ..OtaConfig::default()
    };
    let mut panel = Prepare::new(&root, None, BOARD, None, target);
    panel.set_tool(fake("smpmgr"));
    panel.requirements.iter_mut().for_each(|state| {
        state.probe = ToolProbe::Present("0.19.0".to_string());
    });
    assert!(panel.start().finished);

    let fragment = read(&root, "boards/xiao_esp32c3.conf");
    assert!(fragment.contains("CONFIG_MCUMGR_TRANSPORT_UART=y"));
    assert!(!fragment.contains("CONFIG_MCUMGR_TRANSPORT_UDP=y"));
    assert!(
        !fragment.contains("NET_MAX_CONN"),
        "networking tweaks are UDP's"
    );
    let toml = read(&root, "chiptui.toml");
    assert!(toml.contains("transport = \"serial\""));
    let _ = std::fs::remove_dir_all(&root);
}

/// The build's `.config` is the only place that says whether the transport
/// actually survived.
///
/// Every `MCUMGR_TRANSPORT_*` symbol is a `depends on`, never a `select`
/// (read from `subsys/mgmt/mcumgr/transport/Kconfig.udp`: `NET_UDP` and
/// `NET_SOCKETS`), so a project with no network stack gets it dropped to
/// `n` — the build succeeds, every prepare step reports `Done`, `smpmgr` is
/// correct, and the board simply never answers. It is the one failure in
/// this flow with no symptom of its own.
#[test]
fn the_transport_check_reads_the_built_config_back() {
    use chiptui::ota::prepare::TransportCheck;

    let root = temp_project("transport-check");

    // No build at all: nothing checked, and like the slot check that must
    // never block.
    let (panel, _) = prepare(&root, "smpmgr");
    assert_eq!(panel.transport, TransportCheck::NotChecked);
    assert!(!panel.transport.blocks());

    // A build whose .config carries the symbol.
    let dir = root.join("build/zephyr");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".config"),
        "CONFIG_NET_UDP=y\nCONFIG_MCUMGR_TRANSPORT_UDP=y\n",
    )
    .unwrap();
    let panel = Prepare::new(&root, None, BOARD, Some("build".to_string()), config());
    assert!(
        matches!(panel.transport, TransportCheck::Enabled(_)),
        "{:?}",
        panel.transport
    );

    // The same build without it, and with no fragment written yet: the
    // .config cannot have known about a block that does not exist, so this
    // is `Stale`, not an accusation of an unmet dependency.
    std::fs::write(dir.join(".config"), "CONFIG_NET_UDP=y\n").unwrap();
    let panel = Prepare::new(&root, None, BOARD, Some("build".to_string()), config());
    assert!(
        matches!(panel.transport, TransportCheck::Stale(_)),
        "no fragment yet means the build could not have carried it: {:?}",
        panel.transport
    );

    // Write the fragment, then make the .config newer than it. Now the
    // build *did* see the block and dropped the symbol anyway --- the real
    // trap, and the row that names it.
    let (mut panel, mut processes) = prepare(&root, "smpmgr");
    panel.probe_requirements(&mut processes);
    settle_probes(&mut panel, &mut processes);
    assert!(panel.start().finished);
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(dir.join(".config"), "CONFIG_NET_UDP=y\n").unwrap();

    let panel = Prepare::new(&root, None, BOARD, Some("build".to_string()), config());
    assert!(
        matches!(panel.transport, TransportCheck::Dropped(_)),
        "a current build without the symbol is the trap: {:?}",
        panel.transport
    );
    // It reports and never blocks: the project's configuration is the
    // user's to fix, and `p` proves the answer in a second either way.
    assert!(!panel.transport.blocks());
    let _ = std::fs::remove_dir_all(&root);
}

/// The template carries each transport's *own* dependencies, and stops
/// exactly where the application's architecture begins.
#[test]
fn the_kconfig_tiers_carry_their_transports_dependencies() {
    use chiptui::ota::prepare::kconfig_body;

    // Serial: the driver and the framing the transport is made of. Nothing
    // does `select UART_MCUMGR`, so without these the symbol drops.
    let serial = kconfig_body(Transport::Serial);
    for symbol in [
        "CONFIG_MCUMGR_TRANSPORT_UART=y",
        "CONFIG_UART_MCUMGR=y",
        "CONFIG_BASE64=y",
        "CONFIG_CONSOLE=y",
    ] {
        assert!(serial.contains(symbol), "serial needs {symbol}:\n{serial}");
    }

    // BLE already did this, and is the precedent the serial tier follows.
    let ble = kconfig_body(Transport::Ble);
    assert!(ble.contains("CONFIG_BT_PERIPHERAL=y"));

    // UDP deliberately does *not* choose a network stack --- which one, and
    // DHCP versus static, is the application's architecture. It says so
    // instead, and nothing it emits is an uncommented networking symbol.
    let udp = kconfig_body(Transport::Udp);
    assert!(udp.contains("CONFIG_MCUMGR_TRANSPORT_UDP=y"));
    assert!(
        udp.contains("bring your own"),
        "the header states the boundary:\n{udp}"
    );
    for uncommitted in ["CONFIG_NETWORKING=y", "CONFIG_NET_UDP=y", "CONFIG_WIFI=y"] {
        assert!(
            !udp.lines().any(|line| line.trim() == uncommitted),
            "{uncommitted} is the project's decision, not ours:\n{udp}"
        );
    }
}

/// A sysbuild build's *application domain* is the authority, and the top
/// level must not be read instead.
///
/// Pinned against the reference project's real shape: `build/zephyr/.config`
/// is sysbuild's own configuration and carries **none** of the
/// application's symbols, while `build/<app>/zephyr/.config` has
/// `CONFIG_MCUMGR_TRANSPORT_UDP=y`. Deciding by "does the top-level file
/// exist" --- which is how [`SlotCheck`] picks, and is safe there only
/// because a sysbuild top level has no `zephyr.dts` --- read the wrong file
/// and reported `Dropped` for a project that works.
#[test]
fn the_transport_check_reads_the_application_domain_not_sysbuilds_own_config() {
    use chiptui::ota::prepare::TransportCheck;

    let root = temp_project("transport-domain");
    let build = root.join("build");

    // The sysbuild top level: its own .config, without the app's symbols.
    std::fs::create_dir_all(build.join("zephyr")).unwrap();
    std::fs::write(
        build.join("zephyr/.config"),
        "CONFIG_SB_BOOTLOADER_MCUBOOT=y\n",
    )
    .unwrap();
    std::fs::write(
        build.join("domains.yaml"),
        "default: app\nbuild_dir: /build\ndomains:\n  - name: app\n    build_dir: /build/app\nflash_order:\n  - app\n",
    )
    .unwrap();

    // The application domain, which is where the transport actually lives.
    std::fs::create_dir_all(build.join("app/zephyr")).unwrap();
    std::fs::write(
        build.join("app/zephyr/.config"),
        "CONFIG_NET_UDP=y\nCONFIG_NET_SOCKETS=y\nCONFIG_MCUMGR_TRANSPORT_UDP=y\n",
    )
    .unwrap();

    let panel = Prepare::new(&root, None, BOARD, Some("build".to_string()), config());
    match &panel.transport {
        TransportCheck::Enabled(path) => assert!(
            path.ends_with("build/app/zephyr/.config"),
            "the app domain's config is the one read: {}",
            path.display()
        ),
        other => panic!("a domain that carries the transport reads Enabled, not {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}
