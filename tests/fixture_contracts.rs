//! Load-bearing CLI contracts of the fakes themselves. No hardware or network.
#![cfg(unix)]

use std::process::{Command, Output};

mod common;
use common::{TempDir, fake};

fn run(tool: &str, args: &[&str], root: &TempDir) -> Output {
    Command::new(fake(tool))
        .args(args)
        .current_dir(root.path())
        .env("HOME", root.path())
        .output()
        .expect("fixture starts")
}

#[test]
fn west_rejects_unknown_commands_options_and_missing_values() {
    let dir = TempDir::new("west-invalid");
    for args in [
        vec!["invented"],
        vec!["build", "--invented"],
        vec!["build", "-b"],
        vec!["build", "-d", ""],
        vec!["boards", "-f"],
        vec!["shields", "--invented"],
        vec!["flash", "--invented"],
        vec!["sdk", "install", "-b"],
        vec!["sdk", "install", "-b", ".", "-t"],
        vec!["sdk", "install", "-b", ".", "-t", "--invented"],
        vec!["sdk", "install", "-b", ".", "--invented"],
        vec!["sdk", "install", "-d", ".."],
    ] {
        let output = run("west", &args, &dir);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(!output.stderr.is_empty(), "no explanation for {args:?}");
    }
}

#[test]
fn west_sdk_toolchains_stop_at_options_and_repeated_groups_replace() {
    for (tag, args, arm) in [
        (
            "sdk-two",
            vec![
                "sdk",
                "install",
                "-t",
                "arm-zephyr-eabi",
                "riscv64-zephyr-elf",
                "-b",
                ".",
            ],
            true,
        ),
        (
            "sdk-repeated",
            vec![
                "sdk",
                "install",
                "-t",
                "arm-zephyr-eabi",
                "-t",
                "riscv64-zephyr-elf",
                "-b",
                ".",
            ],
            false,
        ),
    ] {
        let dir = TempDir::new(tag);
        let output = run("west", &args, &dir);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let gnu = dir.join("zephyr-sdk-0.17.0/gnu");
        assert_eq!(gnu.join("arm-zephyr-eabi").is_dir(), arm);
        assert!(gnu.join("riscv64-zephyr-elf").is_dir());
        assert_eq!(
            std::fs::read_dir(gnu).unwrap().count(),
            if arm { 2 } else { 1 }
        );
    }
}

#[test]
fn esptool_validates_pairs_without_confusing_filenames_with_offsets() {
    let dir = TempDir::new("esptool-pairs");
    let filename = "firmware 0x1000.bin";
    std::fs::write(dir.join(filename), b"image").unwrap();
    for args in [
        vec!["write-flash"],
        vec!["write-flash", "0x1000"],
        vec!["write-flash", filename],
        vec!["write-flash", "", filename],
        vec!["write-flash", "not-an-offset", filename],
        vec!["write-flash", "0x1000", filename, "0x2000"],
        vec!["write-flash", "--invented", "0x1000", filename],
        vec!["write-flash", "--flash-mode"],
        vec!["write-flash", "0x1000", "absent.bin"],
    ] {
        let output = run("esptool", &args, &dir);
        assert!(!output.status.success(), "accepted {args:?}");
    }
    // cli_util.arg_auto_int is int(value, 0), not a hexadecimal-only check.
    for offset in ["0x1000", "4096", "0o10000", "0b1000000000000"] {
        let output = run(
            "esptool",
            &[
                "--port",
                "/dev/fake",
                "write-flash",
                offset,
                filename,
                "0x2000",
                filename,
            ],
            &dir,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
