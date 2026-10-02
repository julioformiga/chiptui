//! Build directories are configurations, including same-board siblings.

use chiptui::backend::{
    BuildKind,
    zephyr::{ZephyrBackend, configuration, variants},
};
use chiptui::build::BuildPanel;
use std::path::Path;
use time::UtcOffset;

mod common;
use common::TempDir;

fn cache(root: &Path, dir: &str, board: &str, extra: &str) {
    let path = root.join(dir);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join("CMakeCache.txt"),
        format!(
            "CACHED_BOARD:STRING={board}\nAPPLICATION_SOURCE_DIR:PATH={}\n{extra}",
            root.display()
        ),
    )
    .unwrap();
}

#[test]
fn discovery_keeps_same_board_builds_and_rejects_foreign_and_unconfigured_directories() {
    let dir = TempDir::new("build-discovery");
    let root = dir.path();
    cache(
        root,
        "build_rev_a",
        "nrf52840dk/nrf52840",
        "DTC_OVERLAY_FILE:STRING=rev_a.overlay\n",
    );
    cache(
        root,
        "build_rev_b",
        "nrf52840dk/nrf52840",
        "DTC_OVERLAY_FILE:STRING=rev_b.overlay\n",
    );
    cache(root, "output", "native_sim/native/64", "");
    std::fs::create_dir_all(root.join("build_empty")).unwrap();
    std::fs::create_dir_all(root.join("boards")).unwrap();
    std::fs::write(root.join("boards/native_sim_native_64.overlay"), "").unwrap();
    cache(
        root,
        "build_foreign",
        "nrf52840dk/nrf52840",
        &format!("APP_DIR:PATH={}\n", root.join("other-app").display()),
    );
    let found = variants::variants(root, None, &[], &["native_sim/native/64".into()]);
    assert_eq!(
        found
            .iter()
            .map(|v| v.build_dir.as_str())
            .collect::<Vec<_>>(),
        ["build_rev_a", "build_rev_b"]
    );
}

#[test]
fn configured_rebuild_replays_inputs_as_arguments_without_project_default_leakage() {
    let dir = TempDir::new("build-replay");
    let root = dir.path();
    cache(
        root,
        "build_rev_b",
        "nrf52840dk/nrf52840",
        concat!(
            "SHIELD:STRING=nrf7002ek\n",
            "CONF_FILE:STRING=prj.conf;config/rev b.conf\n",
            "DTC_OVERLAY_FILE:STRING=overlays/rev b.overlay\n",
            "EXTRA_CONF_FILE:STRING=\n",
            "//No help, variable specified on the command line.\n",
            "MY_PRODUCT:STRING=variant B;test\n",
            "CMAKE_C_COMPILER:FILEPATH=/derived/compiler\n"
        ),
    );
    let mut panel = BuildPanel::new(root, UtcOffset::UTC);
    panel.build_args = vec![
        "-DDTC_OVERLAY_FILE=wrong.overlay".into(),
        "-DEXTRA_CONF_FILE=wrong.conf".into(),
    ];
    panel.set_variants(variants::variants(root, None, &[], &[]));
    let command = panel
        .checked_command(BuildKind::Rebuild, &ZephyrBackend)
        .unwrap();
    let args = command.args_slice();
    assert!(args.windows(2).any(|a| a == ["-d", "build_rev_b"]));
    assert!(args.windows(2).any(|a| a == ["-b", "nrf52840dk/nrf52840"]));
    assert!(args.windows(2).any(|a| a == ["--shield", "nrf7002ek"]));
    for expected in [
        "--pristine=always",
        "--no-sysbuild",
        "-DCONF_FILE=prj.conf;config/rev b.conf",
        "-DDTC_OVERLAY_FILE=overlays/rev b.overlay",
        "-DEXTRA_CONF_FILE=",
        "-DMY_PRODUCT=variant B;test",
    ] {
        assert!(
            args.iter().any(|arg| arg == expected),
            "{expected}: {args:?}"
        );
    }
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("wrong") || arg.contains("/derived/compiler"))
    );
    let incremental = panel
        .checked_command(BuildKind::Build, &ZephyrBackend)
        .unwrap();
    assert_eq!(incremental.args_slice(), ["build", "-d", "build_rev_b"]);
}

#[test]
fn sysbuild_is_one_choice_and_rebuild_preserves_top_level_and_image_inputs() {
    let dir = TempDir::new("build-sysbuild-choice");
    let root = dir.path();
    cache(
        root,
        "build_ota",
        "nrf52840dk/nrf52840",
        concat!(
            "SB_CONF_FILE:STRING=sysbuild.conf\n",
            "//No help, variable specified on the command line.\n",
            "application_EXTRA_CONF_FILE:UNINITIALIZED=ota.conf\n"
        ),
    );
    cache(root, "build_ota/application", "nrf52840dk/nrf52840", "");
    cache(root, "build_ota/mcuboot", "nrf52840dk/nrf52840", "");
    std::fs::write(
        root.join("build_ota/domains.yaml"),
        "default: application\nflash_order:\n  - mcuboot\n  - application\n",
    )
    .unwrap();
    let found = variants::variants(root, None, &[], &[]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].build_dir, "build_ota");
    let mut panel = BuildPanel::new(root, UtcOffset::UTC);
    panel.set_variants(found);
    let cmd = panel
        .checked_command(BuildKind::Rebuild, &ZephyrBackend)
        .unwrap();
    assert!(cmd.args_slice().iter().any(|s| s == "--sysbuild"));
    assert!(
        cmd.args_slice()
            .iter()
            .any(|s| s == "-Dapplication_EXTRA_CONF_FILE=ota.conf")
    );
    std::fs::write(root.join("build_ota/domains.yaml"), "broken").unwrap();
    assert!(
        panel
            .checked_command(BuildKind::Rebuild, &ZephyrBackend)
            .unwrap_err()
            .contains("domains.yaml")
    );
    assert!(root.join("build_ota/CMakeCache.txt").is_file());
}

#[test]
fn an_explicit_application_subdirectory_is_respected() {
    let dir = TempDir::new("build-app-source");
    let root = dir.path();
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    cache(
        root,
        "build_app",
        "nrf52840dk/nrf52840",
        &format!("APP_DIR:PATH={}\n", app.display()),
    );
    assert!(configuration::belongs_to(root, Some(&app), "build_app"));
    assert!(!configuration::belongs_to(root, None, "build_app"));
}

#[test]
fn multiple_simulators_are_independent_and_keep_the_last_device_for_flash() {
    let dir = TempDir::new("independent-simulators");
    let root = dir.path();
    for (name, board) in [
        ("build_a", "nrf52840dk/nrf52840"),
        ("build_b", "nrf52840dk/nrf52840"),
        ("build_sim_a", "native_sim/native/64"),
        ("build_sim_b", "native_sim/native/64"),
    ] {
        cache(root, name, board, "");
    }
    let mut panel = BuildPanel::new(root, UtcOffset::UTC);
    panel.set_variants(variants::variants(root, None, &[], &[]));
    assert_eq!(panel.variants.len(), 4);
    assert!(
        panel.lifecycle_ready(true),
        "existing configurations supply their boards"
    );
    panel.select_variant(1);
    panel.select_variant(3);
    assert!(panel.targets_simulator());
    assert_eq!(panel.build_dir, "build_sim_b");
    assert_eq!(panel.flash_build_dir(), "build_b");
    assert_eq!(
        panel
            .checked_command(BuildKind::Build, &ZephyrBackend)
            .unwrap()
            .args_slice(),
        ["build", "-d", "build_sim_b"]
    );
}
