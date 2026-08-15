//! `west` command construction.
//!
//! Every invocation is built here, mirroring the MicroPython backend's
//! `commands.rs` (`SPEC.md` §23: centralized command construction is the
//! mitigation for upstream CLI drift --- when `west` changes a flag, this is
//! the only file that moves).
//!
//! All commands run with the project root as working directory (set by the
//! caller via [`crate::process::Command::current_dir`]), carrying `ZEPHYR_BASE`
//! so `west` finds the workspace even for an application outside it (the
//! decoration's source is [`super::workspace`]; the cwd walk-up alone would
//! only cover in-workspace apps).
//!
//! `BUILD_DIR_DEFAULT` is the conventional `build`; commands mention `-d`
//! only when the panel targets another directory, keeping the common case
//! free of noise while the header still names the target.

use crate::process::Command;

pub const PROGRAM: &str = "west";

/// The build directory `west` uses with no `-d`: the conventional `build` at
/// the project root.
pub const BUILD_DIR_DEFAULT: &str = "build";

/// Appends `-d DIR` when `dir` is not the default (see the module docs).
fn build_dir(command: Command, dir: &str) -> Command {
    if dir == BUILD_DIR_DEFAULT {
        command
    } else {
        command.arg("-d").arg(dir)
    }
}

/// `west build` --- incremental when a configured `build/` exists, otherwise
/// the first configuration, which needs `-b BOARD` (Zephyr has no default
/// target). Passing `-b` on an already-configured build directory is legal
/// only for the *same* board, so it is attached exactly once: on the first
/// build, where it belongs.
pub fn build(board: Option<&str>, build_dir_exists: bool, dir: &str) -> Command {
    let mut command = Command::new(PROGRAM).arg("build");
    command = build_dir(command, dir);
    if !build_dir_exists && let Some(board) = board {
        command = command.arg("-b").arg(board);
    }
    command
}

/// `west build -t clean` --- removes build artifacts through CMake's `clean`
/// target. Requires an existing configured build directory; without one,
/// `west` itself explains what is missing.
pub fn clean(dir: &str) -> Command {
    build_dir(Command::new(PROGRAM).arg("build"), dir)
        .arg("-t")
        .arg("clean")
}

/// `west build --pristine=always [-b BOARD]` --- discards the build directory
/// and configures from scratch, then builds. The board is attached whenever
/// one is known (from the cache of the build directory being discarded), so
/// the fresh configuration lands on the same target instead of asking the
/// user again.
pub fn rebuild(board: Option<&str>, dir: &str) -> Command {
    let mut command = build_dir(Command::new(PROGRAM).arg("build"), dir).arg("--pristine=always");
    if let Some(board) = board {
        command = command.arg("-b").arg(board);
    }
    command
}

/// `west build -t menuconfig` --- the interactive Kconfig editor over the
/// configured build directory. Interactive (ncurses): it is run with the
/// terminal suspended, like `$EDITOR`, never through the piped process
/// manager. Requires a configured build directory; `west` explains what is
/// missing when there is none.
pub fn menuconfig(dir: &str) -> Command {
    build_dir(Command::new(PROGRAM).arg("build"), dir)
        .arg("-t")
        .arg("menuconfig")
}

/// `west update` --- syncs every project in the manifest (`west.yml`) into
/// the workspace. Slow, network-bound, and rewrites the workspace's
/// checkouts, which the workspace pane's confirm quotes before running
/// (`SPEC.md` §15's rule applied to the shared workspace rather than the
/// project).
pub fn update() -> Command {
    Command::new(PROGRAM).arg("update")
}

/// `west sdk list` --- the managed SDK's inventory: installed versions,
/// available releases and their toolchains. Read-only.
pub fn sdk_list() -> Command {
    Command::new(PROGRAM).arg("sdk").arg("list")
}

/// `west boards` --- every known board target, `name description` per line
/// (alphabetical; HWMv2 names carry a `/` qualifier, e.g.
/// `nrf52840dk/nrf52840`). Slow (it walks every board root), so callers run
/// it in the background and parse the accumulated lines at the end.
pub fn boards() -> Command {
    Command::new(PROGRAM).arg("boards")
}

/// `west flash` --- builds (incrementally, if anything changed) and writes
/// the image to the board through the *board's own* runner, which `west`
/// reads from the build directory's `runner.yml` (pyocd, openocd, nrfjprog,
/// jlink, ...). No port or programmer is ever passed here: assuming one
/// would be exactly the mechanism-specific guess `SPEC.md` §10 forbids.
pub fn flash(dir: &str) -> Command {
    build_dir(Command::new(PROGRAM).arg("flash"), dir)
}

/// `west monitor [--port PORT]` --- interactive serial monitor for the
/// flashed board. Runs in a PTY (the app's monitor session), so Ctrl+C
/// reaches west as a real key and exits the session.
pub fn monitor(port: Option<&str>) -> Command {
    let mut command = Command::new(PROGRAM).arg("monitor");
    if let Some(port) = port {
        command = command.arg("--port").arg(port);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_build_carries_the_board() {
        let command = build(Some("nrf52840dk/nrf52840"), false, BUILD_DIR_DEFAULT);
        assert_eq!(command.to_string(), "west build -b nrf52840dk/nrf52840");
    }

    #[test]
    fn incremental_build_never_passes_the_board() {
        // A configured build/ directory already carries the board; `-b` on it
        // is at best redundant and at worst an error when the names disagree.
        let command = build(Some("nrf52840dk/nrf52840"), true, BUILD_DIR_DEFAULT);
        assert_eq!(command.to_string(), "west build");
    }

    #[test]
    fn a_named_build_dir_reaches_every_lifecycle_command() {
        assert_eq!(
            build(None, true, "build-nrf52840").to_string(),
            "west build -d build-nrf52840"
        );
        assert_eq!(
            clean("build-nrf52840").to_string(),
            "west build -d build-nrf52840 -t clean"
        );
        assert_eq!(
            rebuild(Some("nrf52840dk/nrf52840"), "build-nrf52840").to_string(),
            "west build -d build-nrf52840 --pristine=always -b nrf52840dk/nrf52840"
        );
        assert_eq!(
            flash("build-nrf52840").to_string(),
            "west flash -d build-nrf52840"
        );
        // The default stays implicit: `-d build` would be pure noise on the
        // path everyone walks.
        assert_eq!(clean(BUILD_DIR_DEFAULT).to_string(), "west build -t clean");
    }

    #[test]
    fn boardless_first_build_is_left_to_west_to_explain() {
        // No board and no build directory: `west build` fails with its own
        // actionable message ("no board specified"), which is more useful
        // than the panel guessing a substitute.
        assert_eq!(
            build(None, false, BUILD_DIR_DEFAULT).to_string(),
            "west build"
        );
    }

    #[test]
    fn clean_targets_the_existing_build() {
        assert_eq!(clean(BUILD_DIR_DEFAULT).to_string(), "west build -t clean");
    }

    #[test]
    fn menuconfig_targets_the_build_directory() {
        assert_eq!(
            menuconfig(BUILD_DIR_DEFAULT).to_string(),
            "west build -t menuconfig"
        );
        assert_eq!(
            menuconfig("build-release").to_string(),
            "west build -d build-release -t menuconfig"
        );
    }

    #[test]
    fn workspace_commands_stay_bare() {
        assert_eq!(update().to_string(), "west update");
        assert_eq!(sdk_list().to_string(), "west sdk list");
    }

    #[test]
    fn boards_lists_targets() {
        assert_eq!(boards().to_string(), "west boards");
    }

    #[test]
    fn flash_delegates_to_the_boards_runner() {
        assert_eq!(flash(BUILD_DIR_DEFAULT).to_string(), "west flash");
    }

    #[test]
    fn monitor_names_the_port_when_one_is_known() {
        assert_eq!(monitor(None).to_string(), "west monitor");
        assert_eq!(
            monitor(Some("/dev/ttyACM0")).to_string(),
            "west monitor --port /dev/ttyACM0"
        );
    }

    #[test]
    fn rebuild_is_always_pristine() {
        assert_eq!(
            rebuild(Some("nrf52840dk/nrf52840"), BUILD_DIR_DEFAULT).to_string(),
            "west build --pristine=always -b nrf52840dk/nrf52840"
        );
        assert_eq!(
            rebuild(None, BUILD_DIR_DEFAULT).to_string(),
            "west build --pristine=always"
        );
    }
}
