//! `mpremote` command construction.
//!
//! Every invocation is built here. `SPEC.md` §23 names centralised command
//! construction as the mitigation for upstream CLI changes: when `mpremote`
//! changes a flag, this is the only file that moves.

use crate::device::DevicePath;
use crate::process::Command;

pub const PROGRAM: &str = "mpremote";

/// `mpremote devs` --- list available serial ports.
pub fn list_devices() -> Command {
    Command::new(PROGRAM).arg("devs")
}

/// `mpremote [connect PORT] fs --no-verbose ls :PATH`
pub fn list_dir(port: Option<&str>, path: &DevicePath) -> Command {
    filesystem(port, "ls").arg(path.as_arg())
}

/// `mpremote [connect PORT] fs --no-verbose sha256sum :PATH`
pub fn sha256(port: Option<&str>, path: &DevicePath) -> Command {
    filesystem(port, "sha256sum").arg(path.as_arg())
}

/// Common prefix for `fs` sub-commands.
///
/// `--no-verbose` suppresses the `ls :path` header mpremote prints by default,
/// and comes *before* the sub-command: argparse handles a flag preceding the
/// positionals far more predictably than one interleaved between them.
fn filesystem(port: Option<&str>, subcommand: &str) -> Command {
    connect(port).args(["fs", "--no-verbose", subcommand])
}

fn connect(port: Option<&str>) -> Command {
    let command = Command::new(PROGRAM);
    match port {
        Some(port) => command.arg("connect").arg(port),
        // Without an explicit port mpremote auto-connects to the first USB
        // serial device it finds.
        None => command,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_the_root_without_a_port() {
        let command = list_dir(None, &DevicePath::root());
        assert_eq!(command.to_string(), "mpremote fs --no-verbose ls :/");
    }

    #[test]
    fn listing_a_subdirectory_on_a_chosen_port() {
        let command = list_dir(Some("/dev/ttyACM0"), &DevicePath::new("/lib"));
        assert_eq!(
            command.to_string(),
            "mpremote connect /dev/ttyACM0 fs --no-verbose ls :/lib"
        );
    }

    #[test]
    fn the_verbose_flag_precedes_the_subcommand() {
        let args = list_dir(None, &DevicePath::root());
        let args = args.args_slice();
        let flag = args.iter().position(|a| a == "--no-verbose").unwrap();
        let subcommand = args.iter().position(|a| a == "ls").unwrap();
        assert!(flag < subcommand);
    }

    #[test]
    fn hashing_addresses_a_single_file() {
        let command = sha256(
            Some("/dev/ttyUSB0"),
            &DevicePath::new("/lib/umqtt/simple.py"),
        );
        assert_eq!(
            command.to_string(),
            "mpremote connect /dev/ttyUSB0 fs --no-verbose sha256sum :/lib/umqtt/simple.py"
        );
    }

    #[test]
    fn device_enumeration_takes_no_port() {
        assert_eq!(list_devices().to_string(), "mpremote devs");
    }

    #[test]
    fn paths_are_passed_as_one_argument() {
        // A name with a space must not become two arguments (no shell involved).
        let command = list_dir(None, &DevicePath::new("/my data"));
        assert_eq!(command.args_slice().last().unwrap(), ":/my data");
    }
}
