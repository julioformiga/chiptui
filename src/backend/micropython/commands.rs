//! `mpremote` command construction.
//!
//! Every invocation is built here. `SPEC.md` §23 names centralised command
//! construction as the mitigation for upstream CLI changes: when `mpremote`
//! changes a flag, this is the only file that moves.

use std::path::Path;

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

/// `mpremote [connect PORT] fs --no-verbose cat :PATH` --- streams a device
/// file's contents to stdout, for the viewer's "View" action. Nothing is
/// written to local disk: that is what [`download`] is for.
pub fn cat(port: Option<&str>, path: &DevicePath) -> Command {
    filesystem(port, "cat").arg(path.as_arg())
}

/// `mpremote [connect PORT] fs cp :REMOTE LOCAL` --- downloads a device file
/// to `local_path`, byte for byte (mpremote writes it directly, unlike
/// [`cat`]'s captured-stdout preview).
pub fn download(port: Option<&str>, remote: &DevicePath, local_path: &Path) -> Command {
    filesystem(port, "cp")
        .arg(remote.as_arg())
        .arg(local_path.to_string_lossy().into_owned())
}

/// `mpremote [connect PORT] fs cp LOCAL :REMOTE` --- uploads a local file to
/// the device.
pub fn upload(port: Option<&str>, local_path: &Path, remote: &DevicePath) -> Command {
    filesystem(port, "cp")
        .arg(local_path.to_string_lossy().into_owned())
        .arg(remote.as_arg())
}

/// `mpremote [connect PORT] fs --recursive cp LOCAL_DIR :REMOTE_PARENT` ---
/// uploads a whole local directory to the device, nested under
/// `remote_parent` the same way Unix `cp -r src existing_dest_dir` nests
/// `src` under `existing_dest_dir`. `remote_parent` must already exist
/// (mpremote 1.28's recursive `cp` falls back to its non-recursive path,
/// which then fails on the directory, when the destination does not exist
/// and the source has no sub-directories of its own) --- callers always pass
/// the currently-listed device directory, which satisfies that.
pub fn upload_dir(port: Option<&str>, local_dir: &Path, remote_parent: &DevicePath) -> Command {
    filesystem_recursive(port, "cp")
        .arg(local_dir.to_string_lossy().into_owned())
        .arg(remote_parent.as_arg())
}

/// `mpremote [connect PORT] fs --recursive cp :REMOTE_DIR LOCAL_PARENT` ---
/// the download counterpart of [`upload_dir`], same existing-destination
/// requirement.
pub fn download_dir(port: Option<&str>, remote_dir: &DevicePath, local_parent: &Path) -> Command {
    filesystem_recursive(port, "cp")
        .arg(remote_dir.as_arg())
        .arg(local_parent.to_string_lossy().into_owned())
}

/// `mpremote [connect PORT] fs rm :PATH` --- removes a file from the device.
pub fn rm(port: Option<&str>, path: &DevicePath) -> Command {
    filesystem(port, "rm").arg(path.as_arg())
}

/// `mpremote [connect PORT] fs --recursive rm :PATH` --- removes a directory
/// and everything under it.
pub fn rm_recursive(port: Option<&str>, path: &DevicePath) -> Command {
    filesystem_recursive(port, "rm").arg(path.as_arg())
}

/// `mpremote [connect PORT] fs mkdir :PATH` --- creates an empty directory.
/// Not recursive: like the device's own `os.mkdir`, the parent must already
/// exist.
pub fn mkdir(port: Option<&str>, path: &DevicePath) -> Command {
    filesystem(port, "mkdir").arg(path.as_arg())
}

/// `mpremote [connect PORT] fs touch :PATH` --- creates an empty file.
pub fn touch(port: Option<&str>, path: &DevicePath) -> Command {
    filesystem(port, "touch").arg(path.as_arg())
}

/// `mpremote [connect PORT] soft-reset` --- reboots MicroPython without a
/// full hardware reset, re-running `boot.py`/`main.py` so a file just
/// uploaded actually takes effect. Not under `fs`: `soft-reset` is a
/// top-level mpremote command, unlike every other operation here.
///
/// The reboot happens inside raw REPL, where `main.py` is *skipped* --- the
/// script ends up stopped, not restarted.
pub fn soft_reset(port: Option<&str>) -> Command {
    connect(port).arg("soft-reset")
}

/// `mpremote [connect PORT] reset` --- hard reset: the board reboots and runs
/// `boot.py` + `main.py` again, which is what restores a script ChipTUI
/// interrupted. Expands (inside mpremote) to
/// `exec --no-follow "import time, machine; time.sleep_ms(100);
/// machine.reset()"`; `--no-follow` keeps mpremote from waiting on a reply a
/// resetting board never sends.
pub fn hard_reset(port: Option<&str>) -> Command {
    connect(port).arg("reset")
}

/// `mpremote [connect PORT] exec --no-follow "import main"` --- runs
/// `main.py` again *without* rebooting: faster than a reset, but state left
/// behind by the interrupted run (open sockets, half-written peripherals) is
/// still there, so a reset is the cleaner option when in doubt.
pub fn relaunch_main(port: Option<&str>) -> Command {
    connect(port).args(["exec", "--no-follow", "import main"])
}

/// `mpremote [connect PORT] repl` --- starts an interactive REPL/serial monitor.
pub fn repl(port: Option<&str>) -> Command {
    connect(port).arg("repl")
}

/// `mpremote [connect PORT] df` --- filesystem usage per mount (size/used/avail/use%).
/// Not under `fs`: like `soft-reset`, `df` is a top-level mpremote command.
pub fn df(port: Option<&str>) -> Command {
    connect(port).arg("df")
}

/// `mpremote [connect PORT] run LOCAL_PATH` --- executes a local script on the
/// device without copying it to the filesystem, streaming its stdout back.
/// Unlike every other command here, `local_path` is a host path, not a
/// [`DevicePath`]: `run` never touches the device filesystem.
pub fn run(port: Option<&str>, local_path: &Path) -> Command {
    connect(port)
        .arg("run")
        .arg(local_path.to_string_lossy().into_owned())
}

/// `mpremote [connect PORT] mip install SPEC [SPEC ...]` --- installs
/// packages (a name, `name@version`, or a `github:`/`gitlab:`/URL spec)
/// into the device's `/lib`, downloading on the host and writing over the
/// serial connection. Not under `fs`: `mip` is a top-level mpremote
/// command. mpremote 1.28 has no `-r` flag --- a requirements file is
/// parsed by the caller ([`crate::backend::micropython::deps`]) and its
/// specifications passed as arguments, one command per file. Already
/// installed files are skipped by mip itself.
pub fn mip_install(port: Option<&str>, packages: &[String]) -> Command {
    connect(port)
        .args(["mip", "install"])
        .args(packages.iter().cloned())
}

/// Common prefix for `fs` sub-commands.
///
/// `--no-verbose` suppresses the `ls :path` header mpremote prints by default,
/// and comes *before* the sub-command: argparse handles a flag preceding the
/// positionals far more predictably than one interleaved between them.
fn filesystem(port: Option<&str>, subcommand: &str) -> Command {
    connect(port).args(["fs", "--no-verbose", subcommand])
}

/// Same as [`filesystem`], with `--recursive` set --- required by `cp`/`rm`
/// to operate on a directory instead of a single file.
fn filesystem_recursive(port: Option<&str>, subcommand: &str) -> Command {
    connect(port).args(["fs", "--no-verbose", "--recursive", subcommand])
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
    fn cat_addresses_a_single_file() {
        let command = cat(None, &DevicePath::new("/main.py"));
        assert_eq!(
            command.to_string(),
            "mpremote fs --no-verbose cat :/main.py"
        );
    }

    #[test]
    fn download_copies_from_the_device_to_a_local_path() {
        let command = download(
            Some("/dev/ttyACM0"),
            &DevicePath::new("/main.py"),
            Path::new("/home/dev/project/src/main.py"),
        );
        assert_eq!(
            command.to_string(),
            "mpremote connect /dev/ttyACM0 fs --no-verbose cp :/main.py /home/dev/project/src/main.py"
        );
    }

    #[test]
    fn upload_copies_from_a_local_path_to_the_device() {
        let command = upload(
            None,
            Path::new("/home/dev/project/src/main.py"),
            &DevicePath::new("/main.py"),
        );
        assert_eq!(
            command.to_string(),
            "mpremote fs --no-verbose cp /home/dev/project/src/main.py :/main.py"
        );
    }

    #[test]
    fn upload_dir_targets_the_existing_parent_not_a_joined_path() {
        let command = upload_dir(
            None,
            Path::new("/home/dev/project/src/lib"),
            &DevicePath::root(),
        );
        assert_eq!(
            command.to_string(),
            "mpremote fs --no-verbose --recursive cp /home/dev/project/src/lib :/"
        );
    }

    #[test]
    fn download_dir_targets_the_existing_local_parent() {
        let command = download_dir(
            Some("/dev/ttyACM0"),
            &DevicePath::new("/lib"),
            Path::new("/home/dev/project/src"),
        );
        assert_eq!(
            command.to_string(),
            "mpremote connect /dev/ttyACM0 fs --no-verbose --recursive cp :/lib /home/dev/project/src"
        );
    }

    #[test]
    fn rm_recursive_sets_the_flag_before_the_subcommand() {
        let command = rm_recursive(None, &DevicePath::new("/lib"));
        assert_eq!(
            command.to_string(),
            "mpremote fs --no-verbose --recursive rm :/lib"
        );
    }

    #[test]
    fn mkdir_addresses_a_single_directory() {
        let command = mkdir(None, &DevicePath::new("/lib"));
        assert_eq!(command.to_string(), "mpremote fs --no-verbose mkdir :/lib");
    }

    #[test]
    fn touch_addresses_a_single_file() {
        let command = touch(Some("/dev/ttyACM0"), &DevicePath::new("/lib/__init__.py"));
        assert_eq!(
            command.to_string(),
            "mpremote connect /dev/ttyACM0 fs --no-verbose touch :/lib/__init__.py"
        );
    }

    #[test]
    fn soft_reset_takes_no_path() {
        assert_eq!(soft_reset(None).to_string(), "mpremote soft-reset");
        assert_eq!(
            soft_reset(Some("/dev/ttyACM0")).to_string(),
            "mpremote connect /dev/ttyACM0 soft-reset"
        );
    }

    #[test]
    fn a_hard_reset_takes_no_path() {
        assert_eq!(hard_reset(None).to_string(), "mpremote reset");
        assert_eq!(
            hard_reset(Some("/dev/ttyACM0")).to_string(),
            "mpremote connect /dev/ttyACM0 reset"
        );
    }

    #[test]
    fn relaunching_main_runs_it_without_waiting() {
        assert_eq!(
            relaunch_main(None).to_string(),
            "mpremote exec --no-follow \"import main\""
        );
        assert_eq!(
            relaunch_main(Some("/dev/ttyACM0")).to_string(),
            "mpremote connect /dev/ttyACM0 exec --no-follow \"import main\""
        );
        // The code is one argument, not three: no shell is involved.
        assert_eq!(
            relaunch_main(None).args_slice(),
            ["exec", "--no-follow", "import main"]
        );
    }

    #[test]
    fn device_enumeration_takes_no_port() {
        assert_eq!(list_devices().to_string(), "mpremote devs");
    }

    #[test]
    fn df_takes_no_path() {
        assert_eq!(df(None).to_string(), "mpremote df");
        assert_eq!(
            df(Some("/dev/ttyACM0")).to_string(),
            "mpremote connect /dev/ttyACM0 df"
        );
    }

    #[test]
    fn run_executes_a_local_script_without_a_device_path() {
        let command = run(None, Path::new("/home/dev/project/src/main.py"));
        assert_eq!(
            command.to_string(),
            "mpremote run /home/dev/project/src/main.py"
        );
        let command = run(
            Some("/dev/ttyACM0"),
            Path::new("/home/dev/project/src/main.py"),
        );
        assert_eq!(
            command.to_string(),
            "mpremote connect /dev/ttyACM0 run /home/dev/project/src/main.py"
        );
    }

    #[test]
    fn mip_install_takes_package_specs() {
        let command = mip_install(None, &["urequests".to_string()]);
        assert_eq!(command.to_string(), "mpremote mip install urequests");
        let specs = ["github:org/repo".to_string(), "pkg@1.2.3".to_string()];
        let command = mip_install(Some("/dev/ttyACM0"), &specs);
        assert_eq!(
            command.to_string(),
            "mpremote connect /dev/ttyACM0 mip install github:org/repo pkg@1.2.3"
        );
    }

    #[test]
    fn paths_are_passed_as_one_argument() {
        // A name with a space must not become two arguments (no shell involved).
        let command = list_dir(None, &DevicePath::new("/my data"));
        assert_eq!(command.args_slice().last().unwrap(), ":/my data");
    }
}
