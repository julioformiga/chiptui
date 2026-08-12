//! `esptool` command construction.
//!
//! Same reasoning as `micropython::commands`: every invocation is built here,
//! so an upstream flag change is a one-file fix. Verified against `esptool`
//! v5.3.1's Click-based CLI (`esptool [OPTIONS] COMMAND [ARGS]...`): `--port`
//! and `--chip` are *global* options and must precede the sub-command, unlike
//! `write-flash`/`verify-flash`'s own flags which follow it.

use std::path::Path;

use super::{ChipFamily, FlashOptions};
use crate::process::Command;

pub const PROGRAM: &str = "esptool";

/// `esptool [--port PORT] [--chip CHIP] chip-id`
pub fn chip_id(port: Option<&str>) -> Command {
    global(port, None).arg("chip-id")
}

/// `esptool [--port PORT] [--chip CHIP] flash-id`
pub fn flash_id(port: Option<&str>) -> Command {
    global(port, None).arg("flash-id")
}

/// `esptool [--port PORT] [--chip CHIP] erase-flash`
pub fn erase_flash(port: Option<&str>) -> Command {
    global(port, None).arg("erase-flash")
}

/// `esptool [--port PORT] [--chip CHIP] run` --- starts the application
/// already in flash, esptool's closest equivalent to a plain device reset.
pub fn reset(port: Option<&str>) -> Command {
    global(port, None).arg("run")
}

/// `esptool [--port PORT] [--chip CHIP] verify-flash OFFSET FILE`
pub fn verify_flash(
    port: Option<&str>,
    chip: Option<ChipFamily>,
    offset: &str,
    file: &Path,
) -> Command {
    global(port, chip)
        .arg("verify-flash")
        .arg(offset)
        .arg(file.to_string_lossy().into_owned())
}

/// `esptool [--port PORT] [--chip CHIP] write-flash [flash options] OFFSET FILE`
pub fn write_flash(
    port: Option<&str>,
    chip: Option<ChipFamily>,
    offset: &str,
    file: &Path,
    options: &FlashOptions,
) -> Command {
    apply_flash_options(global(port, chip).arg("write-flash"), options)
        .arg(offset)
        .arg(file.to_string_lossy().into_owned())
}

/// The global options that must precede the sub-command.
fn global(port: Option<&str>, chip: Option<ChipFamily>) -> Command {
    let mut command = Command::new(PROGRAM);
    if let Some(port) = port {
        command = command.arg("--port").arg(port);
    }
    if let Some(chip) = chip {
        command = command.arg("--chip").arg(chip.esptool_id());
    }
    command
}

/// Adds `--flash-mode`/`--flash-freq`/`--flash-size` from the structured
/// preset, then the tokenized custom flags --- skipping a preset flag
/// whenever the matching flag name is already present in the custom text, so
/// the user's own value always wins and nothing is emitted twice.
fn apply_flash_options(mut command: Command, options: &FlashOptions) -> Command {
    let tokens: Vec<&str> = options.extra_args.split_whitespace().collect();
    let has_flag = |names: &[&str]| tokens.iter().any(|token| names.contains(token));

    if let Some(mode) = options.flash_mode
        && !has_flag(&["--flash-mode", "-fm"])
    {
        command = command.arg("--flash-mode").arg(mode.esptool_id());
    }
    if let Some(freq) = options.flash_freq
        && !has_flag(&["--flash-freq", "-ff"])
    {
        command = command.arg("--flash-freq").arg(freq.esptool_id());
    }
    if let Some(size) = options.flash_size
        && !has_flag(&["--flash-size", "-fs"])
    {
        command = command.arg("--flash-size").arg(size.esptool_id());
    }

    command.args(tokens.into_iter().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::micropython::esptool::{FlashMode, FlashSize};

    #[test]
    fn chip_id_takes_only_the_global_options() {
        let command = chip_id(Some("/dev/ttyUSB0"));
        assert_eq!(command.to_string(), "esptool --port /dev/ttyUSB0 chip-id");
    }

    #[test]
    fn port_and_chip_are_omitted_when_unknown() {
        assert_eq!(chip_id(None).to_string(), "esptool chip-id");
    }

    #[test]
    fn erase_flash_includes_the_chip_when_known() {
        let command = erase_flash(Some("/dev/ttyUSB0"));
        assert_eq!(
            command.args_slice(),
            ["--port", "/dev/ttyUSB0", "erase-flash"]
        );
    }

    #[test]
    fn write_flash_places_offset_and_file_last() {
        let command = write_flash(
            Some("/dev/ttyUSB0"),
            Some(ChipFamily::Esp32),
            "0x1000",
            Path::new("firmware.bin"),
            &FlashOptions::default(),
        );
        assert_eq!(
            command.to_string(),
            "esptool --port /dev/ttyUSB0 --chip esp32 write-flash 0x1000 firmware.bin"
        );
    }

    #[test]
    fn write_flash_applies_the_structured_presets() {
        let options = FlashOptions {
            flash_mode: Some(FlashMode::Dio),
            flash_size: Some(FlashSize::Detect),
            ..FlashOptions::default()
        };
        let command = write_flash(None, None, "0x0", Path::new("app.bin"), &options);
        assert_eq!(
            command.to_string(),
            "esptool write-flash --flash-mode dio --flash-size detect 0x0 app.bin"
        );
    }

    #[test]
    fn a_custom_flag_suppresses_the_matching_preset() {
        let options = FlashOptions {
            flash_mode: Some(FlashMode::Dio),
            extra_args: "--flash-mode qio --erase-all".to_string(),
            ..FlashOptions::default()
        };
        let command = write_flash(None, None, "0x0", Path::new("app.bin"), &options);
        // The preset `dio` never appears; the custom `qio` wins, once.
        assert_eq!(
            command.to_string(),
            "esptool write-flash --flash-mode qio --erase-all 0x0 app.bin"
        );
    }

    #[test]
    fn verify_flash_takes_no_flash_options() {
        let command = verify_flash(
            Some("/dev/ttyUSB0"),
            Some(ChipFamily::Esp32C3),
            "0x0",
            Path::new("app.bin"),
        );
        assert_eq!(
            command.to_string(),
            "esptool --port /dev/ttyUSB0 --chip esp32c3 verify-flash 0x0 app.bin"
        );
    }

    #[test]
    fn reset_runs_the_application_in_flash() {
        assert_eq!(reset(None).to_string(), "esptool run");
    }

    #[test]
    fn a_path_with_spaces_is_one_argument() {
        let command = write_flash(
            None,
            None,
            "0x0",
            Path::new("my firmware.bin"),
            &FlashOptions::default(),
        );
        assert_eq!(command.args_slice().last().unwrap(), "my firmware.bin");
    }
}
