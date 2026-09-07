//! How a build directory's images reach the board.
//!
//! `west flash` is the right answer almost everywhere. It reads the board's
//! own `runners.yaml` and drives pyocd, openocd, nrfjprog or jlink, and on
//! those runners a sysbuild build flashes every domain correctly. Assuming
//! anything else --- a port, a programmer, an address --- would be exactly
//! the mechanism-specific guess `SPEC.md` §10 forbids, and this module keeps
//! delegating as its default branch for precisely that reason.
//!
//! It is **not** the right answer on the `esp32` runner, and the failure is
//! silent. That runner honours `--esp-app-address` and ignores
//! `--flash-address`, so on a sysbuild build:
//!
//! * plain `west flash` writes only the default domain --- the application,
//!   at the application's address --- and leaves the boot partition holding
//!   whatever was there before, usually the previous non-MCUboot
//!   application;
//! * `west flash --domain mcuboot` writes the *bootloader* to the
//!   *application's* address.
//!
//! Either way the board boots nothing and prints nothing at all, which reads
//! like dead hardware rather than a flashing mistake. Verified on a Seeed
//! XIAO ESP32-C3: the ROM loader found no valid image at `0x0`, and there is
//! no console output to say so because nothing ran.
//!
//! So on that one runner family the images are written by address, with
//! `esptool` directly. The addresses are **read from the build's own
//! devicetree** ([`super::report::partitions`]) rather than tabulated:
//! `boot_partition` is at `0x0` on the Espressif 4 MB layout and could be
//! elsewhere on a board that reserves a region below it, and a wrong address
//! here produces exactly the silent failure this module exists to prevent.
//!
//! Every unresolvable fact is an `Err` phrased as a sentence naming it,
//! never a fallback to [`FlashPlan::Delegate`] --- on the one family this
//! module exists for, delegating is the broken path.

use std::path::{Path, PathBuf};

use super::domains::Domains;
use super::report::partitions::FlashLayout;
use super::{commands, report, yaml};
use crate::backend::esptool::{self, ChipFamily, FlashOptions};
use crate::process::Command;

/// The runner whose sysbuild handling is broken, and the only one this
/// module treats specially.
const ESP32_RUNNER: &str = "esp32";

/// How the images in a build directory are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlashPlan {
    /// One `west flash [-d DIR]`, the board's own runner deciding
    /// everything. Every runner but one, and every non-sysbuild build.
    Delegate,
    /// One `esptool write-flash OFF FILE OFF FILE`, each image at the
    /// address this board's devicetree puts it at. One invocation, so the
    /// board is never left holding half a set.
    Images(Vec<FlashImage>),
}

/// One image and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashImage {
    pub domain: String,
    pub path: PathBuf,
    pub address: u64,
}

/// Decides how `build_dir` under `root` should be flashed.
///
/// The order is: what does the build say its runner is, is it a sysbuild
/// build, and only then --- for the one runner family that needs it --- what
/// do the partitions say.
pub fn plan(root: &Path, build_dir: &str) -> Result<FlashPlan, String> {
    let build_root = root.join(build_dir);
    let domains = Domains::read(&build_root);

    // A sysbuild build puts the application's artifacts one level down, in
    // its own domain directory; a plain build has them at the top.
    let app_dir = match &domains {
        Some(domains) => domains.domain_dir(&domains.default),
        None => build_root.clone(),
    };

    // Unreadable runner configuration means an unconfigured or foreign
    // build directory: today's behaviour, unchanged.
    let Some(runner) = flash_runner(&app_dir) else {
        return Ok(FlashPlan::Delegate);
    };
    if runner != ESP32_RUNNER {
        return Ok(FlashPlan::Delegate);
    }
    // An esp32 build with a single image: `west flash` writes it at the
    // application address and there is no bootloader to misplace.
    let Some(domains) = domains else {
        return Ok(FlashPlan::Delegate);
    };

    let layout = read_layout(&app_dir)?;
    let images = domains.images()?;
    let images = images
        .into_iter()
        .map(|image| {
            let partition = if image.bootloader {
                layout.boot()
            } else {
                layout.slot0()
            };
            let partition = partition.ok_or_else(|| {
                format!(
                    "the devicetree has no {} partition, so there is no \
                     address to write the {} image to",
                    if image.bootloader {
                        "boot_partition"
                    } else {
                        "slot0_partition"
                    },
                    image.name
                )
            })?;
            Ok(FlashImage {
                domain: image.name,
                path: image.path,
                address: partition.address,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    Ok(FlashPlan::Images(images))
}

impl FlashPlan {
    /// The command that performs the plan.
    ///
    /// `esptool` needs a port where `west flash` did not, so an
    /// [`FlashPlan::Images`] plan with no port selected refuses by name
    /// rather than probing every candidate --- the guess this app never
    /// makes (`SPEC.md` §8).
    pub fn command(
        &self,
        build_dir: &str,
        port: Option<&str>,
        chip: Option<ChipFamily>,
        options: &FlashOptions,
    ) -> Result<Command, String> {
        match self {
            Self::Delegate => Ok(commands::flash(build_dir)),
            Self::Images(images) => {
                let port = port.ok_or_else(|| {
                    "no device selected --- flashing this build writes images \
                     by address with esptool, which needs a port"
                        .to_string()
                })?;
                let pairs: Vec<(String, &Path)> = images
                    .iter()
                    .map(|image| (format!("{:#x}", image.address), image.path.as_path()))
                    .collect();
                Ok(esptool::commands::write_flash_images(
                    Some(port),
                    chip,
                    &pairs,
                    options,
                ))
            }
        }
    }
}

/// The `flash-runner` key of the build's own `runners.yaml` --- the file
/// `west flash` itself reads, so this is the build's answer rather than a
/// guess from the board's name.
fn flash_runner(app_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(app_dir.join("zephyr").join("runners.yaml")).ok()?;
    yaml::scalar(&yaml::read_entries(&text), "flash-runner")
}

/// The partitions the build resolved.
fn read_layout(app_dir: &Path) -> Result<FlashLayout, String> {
    let path = app_dir.join("zephyr").join("zephyr.dts");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    Ok(FlashLayout::read(&report::devicetree::parse(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOMAINS: &str = "\
default: app
build_dir: /build
domains:
  - name: app
    build_dir: /build/app
  - name: mcuboot
    build_dir: /build/mcuboot
flash_order:
  - mcuboot
  - app
";

    const DTS: &str = "\
/* node '/soc/flash@0/partitions' defined in board.dtsi:10 */
partitions {
        /* node '/soc/flash@0/partitions/partition@0' defined in board.dtsi:13 */
        boot_partition: partition@0 {
                label = \"mcuboot\";      /* in board.dtsi:15 */
                reg = < 0x0 0x10000 >;   /* in board.dtsi:16 */
        };
        /* node '/soc/flash@0/partitions/partition@20000' defined in board.dtsi:25 */
        slot0_partition: partition@20000 {
                label = \"image-0\";          /* in board.dtsi:27 */
                reg = < 0x20000 0x1c0000 >;  /* in board.dtsi:28 */
        };
        /* node '/soc/flash@0/partitions/partition@1e0000' defined in board.dtsi:31 */
        slot1_partition: partition@1e0000 {
                label = \"image-1\";           /* in board.dtsi:33 */
                reg = < 0x1e0000 0x1c0000 >;  /* in board.dtsi:34 */
        };
};
";

    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        /// A build tree shaped like the one the named runner produces.
        fn new(label: &str, runner: &str, sysbuild: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "chiptui-flashplan-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            let build = root.join("build");
            let app = if sysbuild {
                build.join("app")
            } else {
                build.clone()
            };
            std::fs::create_dir_all(app.join("zephyr")).unwrap();
            std::fs::write(
                app.join("zephyr").join("runners.yaml"),
                format!("runners:\n- {runner}\nflash-runner: {runner}\n"),
            )
            .unwrap();
            std::fs::write(app.join("zephyr").join("zephyr.dts"), DTS).unwrap();
            if sysbuild {
                std::fs::write(build.join("domains.yaml"), DOMAINS).unwrap();
                std::fs::write(app.join("zephyr").join("zephyr.signed.bin"), b"app").unwrap();
                let boot = build.join("mcuboot").join("zephyr");
                std::fs::create_dir_all(&boot).unwrap();
                std::fs::write(boot.join("zephyr.bin"), b"boot").unwrap();
            }
            Self { root }
        }

        fn plan(&self) -> Result<FlashPlan, String> {
            plan(&self.root, "build")
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn an_esp32_sysbuild_build_is_written_by_address() {
        let tree = Tree::new("esp32-sysbuild", "esp32", true);

        let FlashPlan::Images(images) = tree.plan().unwrap() else {
            panic!("the one case west flash gets wrong");
        };

        // Bootloader first, at the boot partition; the signed application
        // second, at slot 0. Both addresses out of the devicetree.
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].domain, "mcuboot");
        assert_eq!(images[0].address, 0x0);
        assert!(images[0].path.ends_with("mcuboot/zephyr/zephyr.bin"));
        assert_eq!(images[1].domain, "app");
        assert_eq!(images[1].address, 0x20000);
        assert!(images[1].path.ends_with("app/zephyr/zephyr.signed.bin"));
    }

    #[test]
    fn the_command_writes_both_images_in_one_invocation() {
        let tree = Tree::new("esp32-command", "esp32", true);
        let plan = tree.plan().unwrap();

        let command = plan
            .command(
                "build",
                Some("/dev/ttyACM0"),
                None,
                &FlashOptions::default(),
            )
            .unwrap();
        let rendered = command.to_string();

        // One connection, one reset: the board is never left holding half a
        // set. And the addresses are visible to the user before it runs.
        assert!(rendered.contains("write-flash"), "{rendered}");
        assert!(rendered.contains("0x0"), "{rendered}");
        assert!(rendered.contains("0x20000"), "{rendered}");
        assert_eq!(rendered.matches("write-flash").count(), 1, "{rendered}");
    }

    #[test]
    fn an_images_plan_without_a_port_refuses_by_name() {
        let tree = Tree::new("esp32-noport", "esp32", true);
        let plan = tree.plan().unwrap();

        let error = plan
            .command("build", None, None, &FlashOptions::default())
            .expect_err("esptool needs a port");

        assert!(error.contains("port"), "{error}");
    }

    #[test]
    fn every_other_runner_keeps_delegating_to_west() {
        // The branch that must not change: these are untested on hardware
        // here, and `west flash` already handles sysbuild correctly on them.
        for runner in ["nrfjprog", "pyocd", "openocd", "jlink"] {
            let tree = Tree::new(runner, runner, true);
            assert_eq!(
                tree.plan().unwrap(),
                FlashPlan::Delegate,
                "{runner} must keep delegating"
            );
        }
    }

    #[test]
    fn an_esp32_build_without_sysbuild_keeps_delegating() {
        let tree = Tree::new("esp32-plain", "esp32", false);

        // One image, written at the application address: exactly what
        // `west flash` already does right.
        assert_eq!(tree.plan().unwrap(), FlashPlan::Delegate);
    }

    #[test]
    fn an_unconfigured_build_directory_keeps_delegating() {
        let root =
            std::env::temp_dir().join(format!("chiptui-flashplan-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("build")).unwrap();

        let planned = plan(&root, "build");
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(planned.unwrap(), FlashPlan::Delegate);
    }

    #[test]
    fn a_devicetree_without_the_boot_partition_refuses_rather_than_guessing() {
        let tree = Tree::new("esp32-noboot", "esp32", true);
        let dts = tree
            .root
            .join("build")
            .join("app")
            .join("zephyr")
            .join("zephyr.dts");
        std::fs::write(&dts, DTS.replace("boot_partition", "other_partition")).unwrap();

        let error = tree.plan().expect_err("no address for the bootloader");

        assert!(error.contains("boot_partition"), "{error}");
        assert!(error.contains("mcuboot"), "{error}");
    }
}
