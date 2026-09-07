//! `<build>/domains.yaml` --- sysbuild's own record of the images it built.
//!
//! A sysbuild build is not one image but several, each configured and linked
//! in its own subdirectory: the bootloader beside the application, and on
//! some SoCs a second core's firmware beside both. Sysbuild writes this file
//! to say which exist, where each landed and in what order they are meant to
//! reach the board, and `west flash --domain` reads it.
//!
//! ```yaml
//! default: blinky
//! build_dir: /home/user/blinky/build
//! domains:
//!   - name: blinky
//!     build_dir: /home/user/blinky/build/blinky
//!   - name: mcuboot
//!     build_dir: /home/user/blinky/build/mcuboot
//! flash_order:
//!   - mcuboot
//!   - blinky
//! ```
//!
//! Two things are read out of it, and the file earns its place twice over:
//!
//! * **Its existence is the honest marker for "this build directory is a
//!   sysbuild one".** A plain `west build` writes no such file. That beats
//!   inferring it from `sysbuild.conf` being in the project, which answers
//!   what the *next* build will do rather than what this directory holds ---
//!   the distinction that matters when a project gained the file after its
//!   build directory was configured.
//! * **`default` names the application domain**, which is how the signed
//!   image is found without guessing that the application is called after
//!   its directory. It frequently is; `project(...)` in `CMakeLists.txt` is
//!   free to say otherwise.
//!
//! Only `default` and `flash_order` are read, and both are shapes
//! [`super::yaml`] handles exactly: a scalar and a sequence of scalars. The
//! `domains` list is a sequence of *mappings*, which that reader flattens
//! lossily --- fine, because every path it would give is derivable from a
//! domain's name, and `flash_order` already carries the names in the order
//! that matters.

use std::path::{Path, PathBuf};

use super::yaml;

/// The file sysbuild writes, and the marker that it ran.
pub const FILE_NAME: &str = "domains.yaml";

/// The domain sysbuild gives the MCUboot image.
///
/// Matched by name rather than by "whichever domain is not the default": on
/// a multi-core SoC the non-default domains include a second core's
/// application, which is not a bootloader and does not belong at the boot
/// partition. Naming the one domain whose placement is known keeps every
/// other case a refusal instead of a wrong address.
pub const BOOTLOADER: &str = "mcuboot";

/// One image a sysbuild build produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainImage {
    /// The domain's name, as `flash_order` lists it.
    pub name: String,
    /// The binary to write --- `zephyr.signed.bin` where the domain has one
    /// (the application, signed for MCUboot to validate), `zephyr.bin`
    /// otherwise (the bootloader itself, which nothing validates).
    pub path: PathBuf,
    /// Whether this is the bootloader, which is what decides the partition
    /// it is written to.
    pub bootloader: bool,
}

/// What `domains.yaml` says about a build directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domains {
    /// The application domain's name, from `default`.
    pub default: String,
    /// The domains to write, in the order sysbuild puts them.
    pub flash_order: Vec<String>,
    /// The build directory this was read from, so image paths can be built
    /// from it rather than from the absolute paths in the file --- which
    /// were written on whatever machine ran the build.
    root: PathBuf,
}

impl Domains {
    /// Reads the file, or `None` when this build directory is not a sysbuild
    /// one --- which is the common case and not an error.
    pub fn read(root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(root.join(FILE_NAME)).ok()?;
        Self::parse(root, &text)
    }

    /// The pure half, so the shapes are testable without a build tree.
    pub fn parse(root: &Path, text: &str) -> Option<Self> {
        let entries = yaml::read_entries(text);
        let default = yaml::scalar(&entries, "default")?;
        let flash_order = yaml::sequence(&entries, "flash_order");
        if flash_order.is_empty() {
            return None;
        }
        Some(Self {
            default,
            flash_order,
            root: root.to_path_buf(),
        })
    }

    /// A domain's build directory: `<build>/<name>`, the layout sysbuild
    /// uses. Derived rather than read, so a build directory that moved (or
    /// was produced on another machine) still resolves.
    pub fn domain_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The images to write, in flash order.
    ///
    /// `Err` names the domain that could not be resolved rather than
    /// dropping it: a partial image set written to a board is worse than a
    /// refusal, because it boots nothing and says nothing about why.
    pub fn images(&self) -> Result<Vec<DomainImage>, String> {
        self.flash_order
            .iter()
            .map(|name| {
                let dir = self.domain_dir(name).join("zephyr");
                let signed = dir.join("zephyr.signed.bin");
                let plain = dir.join("zephyr.bin");
                let path = if signed.is_file() {
                    signed
                } else if plain.is_file() {
                    plain
                } else {
                    return Err(format!(
                        "the {name} image is missing --- {} holds neither \
                         zephyr.signed.bin nor zephyr.bin; build first",
                        dir.display()
                    ));
                };
                Ok(DomainImage {
                    name: name.clone(),
                    path,
                    bootloader: name == BOOTLOADER,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file a real `west build --sysbuild` wrote for an ESP32-C3
    /// application, absolute paths and all.
    const DOMAINS: &str = "\
default: esp32c3-round-display
build_dir: /home/user/esp32c3-round-display/build_ota
domains:
  - name: esp32c3-round-display
    build_dir: /home/user/esp32c3-round-display/build_ota/esp32c3-round-display
  - name: mcuboot
    build_dir: /home/user/esp32c3-round-display/build_ota/mcuboot
flash_order:
  - mcuboot
  - esp32c3-round-display
";

    #[test]
    fn reads_the_application_domain_and_the_flash_order() {
        let domains = Domains::parse(Path::new("/build"), DOMAINS).expect("a sysbuild file");

        // `default` is what keeps the signed image findable without
        // guessing the application's name from its directory.
        assert_eq!(domains.default, "esp32c3-round-display");
        // The bootloader is written first, which is the order sysbuild
        // itself declares.
        assert_eq!(
            domains.flash_order,
            vec!["mcuboot", "esp32c3-round-display"]
        );
    }

    #[test]
    fn domain_directories_come_from_this_build_not_the_recorded_paths() {
        let domains = Domains::parse(Path::new("/elsewhere/build"), DOMAINS).unwrap();

        // The file records the paths of the machine that built it; a build
        // directory that moved still has to resolve.
        assert_eq!(
            domains.domain_dir("mcuboot"),
            Path::new("/elsewhere/build/mcuboot")
        );
    }

    #[test]
    fn a_plain_build_is_not_a_sysbuild_one() {
        assert!(Domains::parse(Path::new("/build"), "").is_none());
        // A file naming a default but no flash order says nothing about
        // what to write, so it is not usable as one either.
        assert!(Domains::parse(Path::new("/build"), "default: blinky\n").is_none());
    }

    #[test]
    fn a_missing_image_names_the_domain_rather_than_being_dropped() {
        let domains = Domains::parse(Path::new("/nonexistent/build"), DOMAINS).unwrap();

        let error = domains.images().expect_err("nothing is built there");

        assert!(error.contains("mcuboot"), "{error}");
        assert!(error.contains("build first"), "{error}");
    }
}
