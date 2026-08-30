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
/// build, where it belongs. `--shield` follows the same rule as `-b`: it is
/// a configuration-time answer, so it rides along only on the first
/// configuration of this build directory (`west` applies a shield through
/// SHIELD at configure time).
pub fn build(
    board: Option<&str>,
    shield: Option<&str>,
    build_dir_exists: bool,
    dir: &str,
) -> Command {
    let mut command = Command::new(PROGRAM).arg("build");
    command = build_dir(command, dir);
    if !build_dir_exists {
        if let Some(board) = board {
            command = command.arg("-b").arg(board);
        }
        command = shield_args(command, shield);
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

/// `west build --pristine=always [-b BOARD] [--shield SHIELD]` --- discards
/// the build directory and configures from scratch, then builds. The board
/// and shield are attached whenever one is known (the board from the cache of
/// the build directory being discarded), so the fresh configuration lands on
/// the same target instead of asking the user again.
pub fn rebuild(board: Option<&str>, shield: Option<&str>, dir: &str) -> Command {
    let mut command = build_dir(Command::new(PROGRAM).arg("build"), dir).arg("--pristine=always");
    if let Some(board) = board {
        command = command.arg("-b").arg(board);
    }
    shield_args(command, shield)
}

/// Appends `--shield NAME` when one is chosen (`--shield` with no value is
/// how `west` *clears* a cached shield, never how it leaves it alone --- so
/// `None` must mean no flag at all).
fn shield_args(command: Command, shield: Option<&str>) -> Command {
    match shield {
        Some(shield) => command.arg("--shield").arg(shield),
        None => command,
    }
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

/// `west build -t dashboard` --- the Zephyr 4.4 build dashboard: one HTML
/// report consolidating the ram/rom reports, the Kconfig symbols, the
/// initialization levels and the device tree, which the target itself
/// opens in the browser. Like every other `-t` target it needs a
/// configured build directory; `west` explains what is missing when there
/// is none, which is why nothing upstream gates on the board answer.
/// `scripts/footprint/size_report` --- the per-symbol memory tree the
/// dashboard's Memory tab reads.
///
/// The one command in this backend that is not `west`. It runs a script out
/// of the Zephyr checkout, which has no console-script shim to embed an
/// interpreter, so the venv's own `python` is named explicitly --- the same
/// interpreter `dashboard.py` reaches through `sys.executable` when it runs
/// this very script (`_create_memory_reports`).
///
/// Three flag forms are load-bearing and are copied from that call site
/// rather than inferred:
///
/// * **`--workspace=<topdir>`**, the equals form. `size_report` builds its
///   parser with `allow_abbrev=False`, so no shortened spelling is accepted.
/// * **`--json <out>/{target}_report.json`**, with `{target}` written
///   *literally*. The script substitutes `rom`/`ram`/`all` into it itself;
///   passing three separate paths, one per target, is the wrong invocation
///   and would leave two of the three files unwritten.
/// * **`--output .`**, which the parser marks `required=True` even though
///   the JSON path never uses it. Omitting it is an argparse error, not a
///   default.
///
/// The output directory is `<build>/dashboard/` --- where Zephyr's own
/// `dashboard` target writes the same three files, so a run here spares the
/// HTML report one and the other way round. It has to *exist*: the script
/// opens each `--json` path with a plain `open(..., "w")` and creates no
/// parent, so a build directory that never ran that target ends the run in a
/// `FileNotFoundError` --- after the whole DWARF walk. The caller
/// ([`crate::build::BuildPanel::size_report_command`]) makes it.
pub fn size_report(
    python: &std::path::Path,
    zephyr_base: &std::path::Path,
    topdir: &std::path::Path,
    elf: &std::path::Path,
    out_dir: &std::path::Path,
) -> Command {
    Command::new(python.display().to_string())
        .arg(
            zephyr_base
                .join("scripts")
                .join("footprint")
                .join("size_report")
                .display()
                .to_string(),
        )
        .arg("-k")
        .arg(elf.display().to_string())
        .arg("-z")
        .arg(zephyr_base.display().to_string())
        .arg(format!("--workspace={}", topdir.display()))
        .arg("--json")
        .arg(out_dir.join("{target}_report.json").display().to_string())
        .arg("--quiet")
        .arg("--output")
        .arg(".")
        .arg("rom")
        .arg("ram")
        .arg("all")
}

pub fn dashboard(dir: &str) -> Command {
    build_dir(Command::new(PROGRAM).arg("build"), dir)
        .arg("-t")
        .arg("dashboard")
}

/// `west update` --- syncs every project in the manifest (`west.yml`) into
/// the workspace. Slow, network-bound, and rewrites the workspace's
/// checkouts, which the workspace pane's confirm quotes before running
/// (`SPEC.md` §15's rule applied to the shared workspace rather than the
/// project).
pub fn update() -> Command {
    Command::new(PROGRAM).arg("update")
}

/// `west boards` --- every known board target, `name description` per line
/// (alphabetical; HWMv2 names carry a `/` qualifier, e.g.
/// `nrf52840dk/nrf52840`). Slow (it walks every board root), so callers run
/// it in the background and parse the accumulated lines at the end.
pub fn boards() -> Command {
    Command::new(PROGRAM).arg("boards")
}

/// `west shields` --- every known shield, `name (description)` per line
/// (alphabetical). Fast compared to `west boards` (it walks the boards'
/// shield roots only), but still a subprocess: callers run it in the
/// background like the board list.
pub fn shields() -> Command {
    Command::new(PROGRAM).arg("shields")
}

/// `west flash` --- builds (incrementally, if anything changed) and writes
/// the image to the board through the *board's own* runner, which `west`
/// reads from the build directory's `runner.yml` (pyocd, openocd, nrfjprog,
/// jlink, ...). No port or programmer is ever passed here: assuming one
/// would be exactly the mechanism-specific guess `SPEC.md` §10 forbids.
pub fn flash(dir: &str) -> Command {
    build_dir(Command::new(PROGRAM).arg("flash"), dir)
}

/// `west espressif monitor -p PORT` --- the serial monitor for an ESP32
/// board, from the west extension the Zephyr workspace itself ships
/// (`hal_espressif`'s `west-commands.yml`, so `west update` installs it).
/// There is no `west monitor`: Zephyr's own extensions are
/// build/flash/debug/sign/... with no monitor among them. The extension
/// wraps ESP-IDF's idf_monitor (backtrace address decoding against the
/// build's ELF), reads its baud from the build's runner configuration
/// instead of a hard-coded one, and --- given the port --- attaches without
/// the esptool probing that would reset the board; portless it *probes
/// every candidate port*, which is exactly the guess this app never makes,
/// so the port always rides along. The command runs with the project root
/// as cwd (the extension searches it for the build directory) and the
/// workspace's west ([`WestEnv::apply`], the caller's decoration).
pub fn monitor(port: &str) -> Command {
    Command::new(PROGRAM)
        .arg("espressif")
        .arg("monitor")
        .arg("-p")
        .arg(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three flag forms that are load-bearing, pinned against the call
    /// `dashboard.py::_create_memory_reports` makes.
    #[test]
    fn the_memory_report_command_matches_the_python_call_site() {
        let command = size_report(
            std::path::Path::new("/ws/.venv/bin/python"),
            std::path::Path::new("/ws/zephyr"),
            std::path::Path::new("/ws"),
            std::path::Path::new("/p/build/zephyr/zephyr.elf"),
            std::path::Path::new("/p/build/dashboard"),
        );
        // `Display` shortens the program to its file name (the venv path is
        // execution detail, the same way the venv `west` reads as `west`) ---
        // the arguments are what a reader checks.
        assert_eq!(
            command.to_string(),
            "python /ws/zephyr/scripts/footprint/size_report \
             -k /p/build/zephyr/zephyr.elf -z /ws/zephyr --workspace=/ws \
             --json /p/build/dashboard/{target}_report.json --quiet --output . rom ram all"
        );
    }

    /// `{target}` is written *literally*: `size_report` substitutes
    /// `rom`/`ram`/`all` itself, and three separate paths would leave two of
    /// the three files unwritten.
    #[test]
    fn the_json_path_carries_the_placeholder_not_a_target() {
        let command = size_report(
            std::path::Path::new("/py"),
            std::path::Path::new("/z"),
            std::path::Path::new("/w"),
            std::path::Path::new("/e.elf"),
            std::path::Path::new("/out"),
        );
        let line = command.to_string();
        assert!(line.contains("--json /out/{target}_report.json"), "{line}");
        assert!(!line.contains("all_report.json"), "{line}");
    }

    /// The equals form, because the parser is built with
    /// `allow_abbrev=False` and the Python writes it that way.
    #[test]
    fn the_workspace_flag_uses_the_equals_form() {
        let line = size_report(
            std::path::Path::new("/py"),
            std::path::Path::new("/z"),
            std::path::Path::new("/top"),
            std::path::Path::new("/e.elf"),
            std::path::Path::new("/out"),
        )
        .to_string();
        assert!(line.contains("--workspace=/top"), "{line}");
        assert!(!line.contains("--workspace /top"), "{line}");
    }

    /// `--output` is `required=True` even though the JSON path never reads
    /// it: omitting it is an argparse error, not a default.
    #[test]
    fn the_required_output_flag_is_always_present() {
        let line = size_report(
            std::path::Path::new("/py"),
            std::path::Path::new("/z"),
            std::path::Path::new("/w"),
            std::path::Path::new("/e.elf"),
            std::path::Path::new("/out"),
        )
        .to_string();
        assert!(line.contains("--output ."), "{line}");
    }

    #[test]
    fn first_build_carries_the_board_and_shield() {
        let command = build(Some("nrf52840dk/nrf52840"), None, false, BUILD_DIR_DEFAULT);
        assert_eq!(command.to_string(), "west build -b nrf52840dk/nrf52840");

        let command = build(
            Some("nrf52840dk/nrf52840"),
            Some("nrf7002ek"),
            false,
            BUILD_DIR_DEFAULT,
        );
        assert_eq!(
            command.to_string(),
            "west build -b nrf52840dk/nrf52840 --shield nrf7002ek"
        );
    }

    #[test]
    fn a_shield_without_a_board_still_reaches_the_first_build() {
        // `-b` is the required answer, `--shield` the optional one; the
        // shield must not wait for the board.
        let command = build(None, Some("link_board_eth"), false, BUILD_DIR_DEFAULT);
        assert_eq!(command.to_string(), "west build --shield link_board_eth");
    }

    #[test]
    fn incremental_build_never_passes_the_board_or_shield() {
        // A configured build/ directory already carries the board and shield;
        // `-b`/`--shield` on it is at best redundant and at worst an error
        // when the names disagree.
        let command = build(
            Some("nrf52840dk/nrf52840"),
            Some("nrf7002ek"),
            true,
            BUILD_DIR_DEFAULT,
        );
        assert_eq!(command.to_string(), "west build");
    }

    #[test]
    fn a_named_build_dir_reaches_every_lifecycle_command() {
        assert_eq!(
            build(None, None, true, "build-nrf52840").to_string(),
            "west build -d build-nrf52840"
        );
        assert_eq!(
            clean("build-nrf52840").to_string(),
            "west build -d build-nrf52840 -t clean"
        );
        assert_eq!(
            rebuild(Some("nrf52840dk/nrf52840"), None, "build-nrf52840").to_string(),
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
            build(None, None, false, BUILD_DIR_DEFAULT).to_string(),
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
    fn dashboard_is_a_build_target_like_the_others() {
        assert_eq!(
            dashboard(BUILD_DIR_DEFAULT).to_string(),
            "west build -t dashboard"
        );
        assert_eq!(
            dashboard("build-release").to_string(),
            "west build -d build-release -t dashboard"
        );
    }

    #[test]
    fn workspace_commands_stay_bare() {
        assert_eq!(update().to_string(), "west update");
    }

    #[test]
    fn boards_and_shields_list_their_targets() {
        assert_eq!(boards().to_string(), "west boards");
        assert_eq!(shields().to_string(), "west shields");
    }

    #[test]
    fn flash_delegates_to_the_boards_runner() {
        assert_eq!(flash(BUILD_DIR_DEFAULT).to_string(), "west flash");
    }

    #[test]
    fn monitor_is_the_workspaces_espressif_extension_with_the_port() {
        // The port always rides along: portless, the extension probes every
        // candidate port with esptool, resetting each board --- the guess
        // this app never makes.
        assert_eq!(
            monitor("/dev/ttyACM0").to_string(),
            "west espressif monitor -p /dev/ttyACM0"
        );
    }

    #[test]
    fn rebuild_is_always_pristine() {
        assert_eq!(
            rebuild(Some("nrf52840dk/nrf52840"), None, BUILD_DIR_DEFAULT).to_string(),
            "west build --pristine=always -b nrf52840dk/nrf52840"
        );
        assert_eq!(
            rebuild(None, None, BUILD_DIR_DEFAULT).to_string(),
            "west build --pristine=always"
        );
        // A pristine rebuild reconfigures, so the shield rides along like
        // the board does.
        assert_eq!(
            rebuild(
                Some("nrf52840dk/nrf52840"),
                Some("nrf7002ek"),
                BUILD_DIR_DEFAULT
            )
            .to_string(),
            "west build --pristine=always -b nrf52840dk/nrf52840 --shield nrf7002ek"
        );
    }
}
