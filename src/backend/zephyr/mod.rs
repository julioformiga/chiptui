//! Zephyr backend.
//!
//! Detection plus the operation surface that goes through `west`: command
//! construction lives in [`commands`], mirroring the MicroPython backend's
//! split (`SPEC.md` §12 --- one seam per tool).

pub mod commands;
pub mod projects;
pub mod report;
pub mod workspace;

use crate::backend::{
    Backend, BackendKind, BuildKind, BuildReportContext, Capabilities, Capability,
};
use crate::project::{DirScan, Signal};

/// Test/sample metadata files used across the Zephyr tree.
const ZEPHYR_METADATA: &[&str] = &["sample.yaml", "testcase.yaml"];

pub struct ZephyrBackend;

impl Backend for ZephyrBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Zephyr
    }

    fn detect(&self, scan: &DirScan) -> Vec<Signal> {
        let mut signals = Vec::new();

        // Decisive: this line is what makes a CMake project a Zephyr
        // application, and it is what separates the two (SPEC.md §7).
        if scan.text_contains("CMakeLists.txt", "find_package(zephyr") {
            signals.push(Signal::new(
                "cmake-zephyr",
                3.0,
                "CMakeLists.txt calls find_package(Zephyr)",
            ));
        } else if scan.has_file("CMakeLists.txt") {
            signals.push(Signal::new(
                "cmake",
                0.25,
                "CMakeLists.txt present (generic CMake)",
            ));
        }

        if scan.has_file("prj.conf") {
            signals.push(Signal::new("prj.conf", 1.5, "Kconfig fragment prj.conf"));
        }
        if scan.has_dir(".west") {
            signals.push(Signal::new(".west", 1.5, ".west/ workspace directory"));
        }
        if scan.has_any_file(&["west.yml", "west.yaml"]) {
            signals.push(Signal::new("west.yml", 1.0, "west manifest"));
        }
        if scan.has_file_with_suffix(".overlay") {
            signals.push(Signal::new("overlay", 0.5, "devicetree overlay"));
        }
        if scan.has_dir("boards") {
            signals.push(Signal::new("boards", 0.5, "boards/ directory"));
        }
        if scan.has_file("Kconfig") {
            signals.push(Signal::new("Kconfig", 0.25, "Kconfig file"));
        }
        if scan.has_any_file(ZEPHYR_METADATA) {
            signals.push(Signal::new(
                "zephyr-metadata",
                0.5,
                "sample/testcase metadata",
            ));
        }

        signals
    }

    fn saturation(&self) -> f32 {
        4.0
    }

    fn capabilities(&self) -> Capabilities {
        // SPEC.md §6: no filesystem/REPL --- the target runs a compiled image.
        Capabilities::from_slice(&[
            Capability::Build,
            Capability::Clean,
            Capability::Flash,
            Capability::Monitor,
            Capability::BoardSelect,
            Capability::ShieldSelect,
            Capability::ProjectSelect,
            Capability::WorkspaceSync,
        ])
    }

    fn required_tools(&self) -> &'static [&'static str] {
        &["west", "cmake", "ninja"]
    }

    /// The three files `west build -b <board>` needs and nothing more: the
    /// `find_package(Zephyr)` call that turns a CMake project into a Zephyr
    /// application, an empty Kconfig fragment, and a `main`. `ZEPHYR_BASE`
    /// is the same variable the build panel exports for every command, so a
    /// project created outside the workspace still finds its Zephyr.
    ///
    /// Deliberately also the shape detection scores highest on
    /// ([`Self::detect`]): a scaffolded project is recognizable on its own
    /// evidence, without depending on the registry that recorded it.
    fn scaffold(&self, name: &str) -> crate::project::Scaffold {
        let name = crate::project::scaffold::safe_name(name);
        crate::project::Scaffold {
            dirs: vec!["src".into()],
            files: vec![
                crate::project::ScaffoldFile::new(
                    "CMakeLists.txt",
                    format!(
                        "cmake_minimum_required(VERSION 3.20.0)\n\
                         \n\
                         find_package(Zephyr REQUIRED HINTS $ENV{{ZEPHYR_BASE}})\n\
                         project({name})\n\
                         \n\
                         target_sources(app PRIVATE src/main.c)\n"
                    ),
                ),
                crate::project::ScaffoldFile::new(
                    "prj.conf",
                    "# Application Kconfig options. Empty means the board's defaults;\n\
                     # `menuconfig` shows what is available for the configured board.\n",
                ),
                crate::project::ScaffoldFile::new(
                    "src/main.c",
                    format!(
                        "#include <zephyr/kernel.h>\n\
                         \n\
                         int main(void)\n\
                         {{\n\
                         \tprintk(\"hello from {name} on %s\\n\", CONFIG_BOARD);\n\
                         \n\
                         \twhile (1) {{\n\
                         \t\tk_sleep(K_SECONDS(1));\n\
                         \t}}\n\
                         \n\
                         \treturn 0;\n\
                         }}\n"
                    ),
                ),
            ],
        }
    }

    fn build_command(
        &self,
        kind: BuildKind,
        board: Option<&str>,
        shield: Option<&str>,
        build_dir_exists: bool,
        build_dir: &str,
    ) -> Option<crate::process::Command> {
        Some(match kind {
            BuildKind::Build => commands::build(board, shield, build_dir_exists, build_dir),
            BuildKind::Clean => commands::clean(build_dir),
            BuildKind::Rebuild => commands::rebuild(board, shield, build_dir),
        })
    }

    fn board_list_command(&self) -> Option<crate::process::Command> {
        Some(commands::boards())
    }

    fn shield_list_command(&self) -> Option<crate::process::Command> {
        Some(commands::shields())
    }

    fn flash_command(&self, build_dir: &str) -> Option<crate::process::Command> {
        Some(commands::flash(build_dir))
    }

    fn menuconfig_command(&self, build_dir: &str) -> Option<crate::process::Command> {
        Some(commands::menuconfig(build_dir))
    }

    fn dashboard_command(&self, build_dir: &str) -> Option<crate::process::Command> {
        Some(commands::dashboard(build_dir))
    }

    fn workspace_update_command(&self) -> Option<crate::process::Command> {
        Some(commands::update())
    }

    fn size_report_command(
        &self,
        ctx: &BuildReportContext<'_>,
    ) -> Result<crate::process::Command, String> {
        if !ctx.elf.is_file() {
            // `size_report` asserts on this itself, but its own message is a
            // Python traceback; the pane can say it in a sentence instead.
            return Err(format!(
                "no {} --- build the project first",
                ctx.elf
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "zephyr.elf".to_string())
            ));
        }
        Ok(commands::size_report(
            ctx.python,
            ctx.zephyr_base,
            ctx.topdir,
            ctx.elf,
            ctx.out_dir,
        ))
    }

    fn monitor_command(
        &self,
        ctx: &crate::backend::MonitorContext<'_>,
    ) -> Result<crate::process::Command, String> {
        use crate::firmware_id::{FirmwareVerdict, FlashFirmware};
        if matches!(
            ctx.firmware,
            Some(FirmwareVerdict::Firmware(FlashFirmware::MicroPython, _))
        ) {
            // Auto-detection: the identification chain read MicroPython off
            // this board's flash, so whatever the project is, the board runs
            // MicroPython --- and mpremote is that firmware's own monitor
            // (protocol-aware where a serial console is only transparent).
            return Ok(crate::backend::micropython::commands::repl(ctx.port));
        }
        match ctx.firmware {
            // Wrong firmware: a monitor would only show the wrong
            // environment's output. Flashing a Zephyr image first is the
            // fix this backend can name.
            Some(FirmwareVerdict::Firmware(FlashFirmware::EspIdf, _)) => {
                return Err(
                    "the device runs ESP-IDF, not Zephyr --- flash a Zephyr image first"
                        .to_string(),
                );
            }
            Some(FirmwareVerdict::Erased) => {
                return Err(
                    "the device's flash is erased --- flash a Zephyr image first".to_string(),
                );
            }
            _ => {}
        }
        let port = ctx
            .port
            .ok_or_else(|| "no device selected (d rescans)".to_string())?;
        let west = ctx.west.ok_or_else(|| {
            "no Zephyr workspace resolved --- the platform monitor runs through its west"
                .to_string()
        })?;
        // The platform decides the form. ESP32 boards have one in the
        // environment itself (`hal_espressif`'s west extension); Zephyr
        // ships no monitor for any other platform, and a generic serial
        // viewer would be a monitor at any cost rather than the
        // environment's own form --- so each missing fact is refused by
        // name, and non-Espressif platforms are refused outright.
        let board = ctx.board.ok_or_else(|| {
            "no board answer --- build (or pick a board) first: the platform \
             monitor reads the build's runner configuration"
                .to_string()
        })?;
        if !ctx.build_configured {
            return Err(
                "no configured build directory --- build first: the platform \
                 monitor reads the build's runner configuration"
                    .to_string(),
            );
        }
        if !is_espressif(board) {
            return Err(format!(
                "the Zephyr environment ships no monitor for {board} --- its \
                 console is a plain serial port, outside the Zephyr environment"
            ));
        }
        let command = match ctx.project_root {
            Some(root) => commands::monitor(port).current_dir(root),
            None => commands::monitor(port),
        };
        Ok(west.apply(command))
    }
}

/// Whether `board` names an ESP32-family target. Every Espressif SoC
/// Zephyr names carries the `esp32` token (`esp32`, `esp32c3`, `esp32s3`,
/// ...), anywhere in the name --- HWMv2 vendor-qualified names put it in
/// the middle (`adafruit_feather_esp32s3/esp32s3/procpu`).
fn is_espressif(board: &str) -> bool {
    board.contains("esp32")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MonitorContext;
    use crate::backend::zephyr::workspace::WestEnv;
    use crate::firmware_id::{FirmwareVerdict, FlashFirmware};

    /// A context with every fact the ESP32 monitor route needs.
    fn esp32_context<'a>(
        west: &'a WestEnv,
        firmware: Option<&'a FirmwareVerdict>,
        board: &'a str,
    ) -> MonitorContext<'a> {
        MonitorContext {
            port: Some("/dev/ttyACM0"),
            firmware,
            board: Some(board),
            build_configured: true,
            west: Some(west),
            project_root: Some(std::path::Path::new("/proj")),
        }
    }

    #[test]
    fn declares_no_filesystem_or_repl_capability() {
        let caps = ZephyrBackend.capabilities();
        assert!(caps.contains(Capability::Build));
        assert!(caps.contains(Capability::Clean));
        assert!(!caps.contains(Capability::Filesystem));
        assert!(!caps.contains(Capability::Repl));
        assert!(!caps.contains(Capability::Upload));
    }

    #[test]
    fn a_board_identified_as_micropython_gets_mpremote_whatever_the_project() {
        let west = WestEnv::from_path();
        let firmware =
            FirmwareVerdict::Firmware(FlashFirmware::MicroPython, Some("v1.28.0".into()));
        let context = MonitorContext {
            firmware: Some(&firmware),
            ..esp32_context(&west, None, "esp32_devkitc_wrover/esp32/procpu")
        };
        assert_eq!(
            ZephyrBackend.monitor_command(&context).unwrap().to_string(),
            "mpremote connect /dev/ttyACM0 repl"
        );
    }

    #[test]
    fn an_esp32_board_gets_the_workspaces_own_espressif_monitor() {
        let west = WestEnv::from_path();
        let zephyr = FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None);
        // An HWMv2 vendor-qualified name: the esp32 token sits in the
        // middle, which is why the check cannot be a prefix.
        let context = esp32_context(
            &west,
            Some(&zephyr),
            "adafruit_feather_esp32s3/esp32s3/procpu",
        );
        assert_eq!(
            ZephyrBackend.monitor_command(&context).unwrap().to_string(),
            "west espressif monitor -p /dev/ttyACM0"
        );
        // Unidentified firmware does not block the route: the board answer
        // is the platform fact.
        let context = esp32_context(&west, None, "esp32c3_devkitm/esp32c3");
        assert!(ZephyrBackend.monitor_command(&context).is_ok());
    }

    #[test]
    fn every_missing_fact_is_refused_by_name() {
        let west = WestEnv::from_path();
        let zephyr = FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None);
        let board = "esp32_devkitc_wrover/esp32/procpu";
        let refusal =
            |context: &MonitorContext<'_>| ZephyrBackend.monitor_command(context).unwrap_err();
        // Wrong firmware is refused even with everything else in place.
        let idf = FirmwareVerdict::Firmware(FlashFirmware::EspIdf, None);
        assert!(refusal(&esp32_context(&west, Some(&idf), board)).contains("ESP-IDF"));
        // No selected port.
        let mut context = esp32_context(&west, Some(&zephyr), board);
        context.port = None;
        assert!(refusal(&context).contains("no device selected"));
        // No workspace: the platform monitor runs through its west.
        let mut context = esp32_context(&west, Some(&zephyr), board);
        context.west = None;
        assert!(refusal(&context).contains("workspace"));
        // No board answer.
        let mut context = esp32_context(&west, Some(&zephyr), board);
        context.board = None;
        assert!(refusal(&context).contains("board"));
        // No configured build directory.
        let mut context = esp32_context(&west, Some(&zephyr), board);
        context.build_configured = false;
        assert!(refusal(&context).contains("build"));
    }

    #[test]
    fn a_non_espressif_platform_is_refused_not_improvised() {
        let west = WestEnv::from_path();
        // Zephyr ships no monitor for nRF, STM32, ... and a generic serial
        // viewer would be a monitor at any cost --- the refusal names the
        // platform so the user can open their own terminal.
        let zephyr = FirmwareVerdict::Firmware(FlashFirmware::Zephyr, None);
        let context = esp32_context(&west, Some(&zephyr), "nrf52840dk/nrf52840");
        let reason = ZephyrBackend.monitor_command(&context).unwrap_err();
        assert!(
            reason.contains("nrf52840dk") && reason.contains("no monitor"),
            "the refusal must name the platform: {reason}"
        );
    }

    #[test]
    fn generic_cmake_does_not_stack_with_the_zephyr_marker() {
        let scan = DirScan::from_parts(
            "/p",
            ["CMakeLists.txt"],
            [],
            [("CMakeLists.txt", "find_package(Zephyr REQUIRED)")],
        );
        let ids: Vec<_> = ZephyrBackend.detect(&scan).iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec!["cmake-zephyr"],
            "generic and Zephyr CMake signals must be exclusive"
        );
    }

    #[test]
    fn generic_cmake_alone_scores_almost_nothing() {
        let scan = DirScan::from_parts(
            "/p",
            ["CMakeLists.txt"],
            [],
            [("CMakeLists.txt", "add_executable(app main.c)")],
        );
        let total: f32 = ZephyrBackend.detect(&scan).iter().map(|s| s.weight).sum();
        assert_eq!(total, 0.25);
    }

    #[test]
    fn both_west_manifest_spellings_are_accepted() {
        for name in ["west.yml", "west.yaml"] {
            let scan = DirScan::from_parts("/p", [name], [], []);
            assert!(
                ZephyrBackend
                    .detect(&scan)
                    .iter()
                    .any(|s| s.id == "west.yml")
            );
        }
    }

    #[test]
    fn any_overlay_file_counts() {
        let scan = DirScan::from_parts("/p", ["nrf52840dk_nrf52840.overlay"], [], []);
        assert!(
            ZephyrBackend
                .detect(&scan)
                .iter()
                .any(|s| s.id == "overlay")
        );
    }
}
