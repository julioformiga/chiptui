//! `west` command construction.
//!
//! Every invocation is built here, mirroring the MicroPython backend's
//! `commands.rs` (`SPEC.md` §23: centralized command construction is the
//! mitigation for upstream CLI drift --- when `west` changes a flag, this is
//! the only file that moves).
//!
//! All commands run with the project root as working directory (set by the
//! caller via [`crate::process::Command::current_dir`]): `west` locates the
//! workspace through the current directory, unlike `mpremote` whose paths are
//! absolute.

use crate::process::Command;

pub const PROGRAM: &str = "west";

/// `west build` --- incremental when a configured `build/` exists, otherwise
/// the first configuration, which needs `-b BOARD` (Zephyr has no default
/// target). Passing `-b` on an already-configured build directory is legal
/// only for the *same* board, so it is attached exactly once: on the first
/// build, where it belongs.
pub fn build(board: Option<&str>, build_dir_exists: bool) -> Command {
    let mut command = Command::new(PROGRAM).arg("build");
    if !build_dir_exists && let Some(board) = board {
        command = command.arg("-b").arg(board);
    }
    command
}

/// `west build -t clean` --- removes build artifacts through CMake's `clean`
/// target. Requires an existing configured build directory; without one,
/// `west` itself explains what is missing.
pub fn clean() -> Command {
    Command::new(PROGRAM).arg("build").arg("-t").arg("clean")
}

/// `west build --pristine=always [-b BOARD]` --- discards the build directory
/// and configures from scratch, then builds. The board is attached whenever
/// one is known (from the cache of the build directory being discarded), so
/// the fresh configuration lands on the same target instead of asking the
/// user again.
pub fn rebuild(board: Option<&str>) -> Command {
    let mut command = Command::new(PROGRAM).arg("build").arg("--pristine=always");
    if let Some(board) = board {
        command = command.arg("-b").arg(board);
    }
    command
}

/// `west boards` --- every known board target, `name description` per line
/// (alphabetical; HWMv2 names carry a `/` qualifier, e.g.
/// `nrf52840dk/nrf52840`). Slow (it walks every board root), so callers run
/// it in the background and parse the accumulated lines at the end.
pub fn boards() -> Command {
    Command::new(PROGRAM).arg("boards")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_build_carries_the_board() {
        let command = build(Some("nrf52840dk/nrf52840"), false);
        assert_eq!(command.to_string(), "west build -b nrf52840dk/nrf52840");
    }

    #[test]
    fn incremental_build_never_passes_the_board() {
        // A configured build/ directory already carries the board; `-b` on it
        // is at best redundant and at worst an error when the names disagree.
        let command = build(Some("nrf52840dk/nrf52840"), true);
        assert_eq!(command.to_string(), "west build");
    }

    #[test]
    fn boardless_first_build_is_left_to_west_to_explain() {
        // No board and no build directory: `west build` fails with its own
        // actionable message ("no board specified"), which is more useful
        // than the panel guessing a substitute.
        assert_eq!(build(None, false).to_string(), "west build");
    }

    #[test]
    fn clean_targets_the_existing_build() {
        assert_eq!(clean().to_string(), "west build -t clean");
    }

    #[test]
    fn boards_lists_targets() {
        assert_eq!(boards().to_string(), "west boards");
    }

    #[test]
    fn rebuild_is_always_pristine() {
        assert_eq!(
            rebuild(Some("nrf52840dk/nrf52840")).to_string(),
            "west build --pristine=always -b nrf52840dk/nrf52840"
        );
        assert_eq!(rebuild(None).to_string(), "west build --pristine=always");
    }
}
