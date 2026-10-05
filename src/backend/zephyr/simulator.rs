//! Explicit LVGL + SDL project preparation. No source migration, downloads or
//! system-wide installation: existing files are retained and named in review.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use super::variants::{Variant, VariantOrigin};
use crate::project::{Scaffold, ScaffoldFile, config, scaffold};

pub const DEFAULT_TARGET: &str = "native_sim/native/64";
pub const TARGETS: [&str; 2] = [DEFAULT_TARGET, "native_sim/native"];
pub const REQUIREMENTS: &str = "Requires Linux, a graphical session and SDL2 development files (pkg-config --exists sdl2). Nothing is installed automatically.";
pub const GUIDANCE: &str = "Existing sources and prj.conf are preserved. Move hardware-only options into board fragments and adapt peripheral access before building the simulator.";

const CONF: &str = "# LVGL + SDL host configuration. Hardware-only options belong in the device fragment.\n\
CONFIG_DISPLAY=y\n\
CONFIG_INPUT=y\n\
CONFIG_LVGL=y\n\
CONFIG_LV_COLOR_DEPTH_32=y\n\
CONFIG_LV_Z_MEM_POOL_SIZE=16384\n\
CONFIG_LV_USE_LABEL=y\n\
CONFIG_LV_FONT_MONTSERRAT_14=y\n\
CONFIG_MAIN_STACK_SIZE=4096\n";

const OVERLAY: &str = "/* SDL window and mouse input; uses native_sim's own devices. */\n\
/ {\n\
\tchosen {\n\
\t\tzephyr,display = &sdl_dc;\n\
\t\tzephyr,touch = &input_sdl_touch;\n\
\t};\n\
};\n\
\n\
&sdl_dc {\n\
\tstatus = \"okay\";\n\
\twidth = <320>;\n\
\theight = <240>;\n\
};\n\
\n\
&input_sdl_touch {\n\
\tstatus = \"okay\";\n\
};\n";

const MAIN: &str = "#include <zephyr/kernel.h>\n\
\n\
#if defined(CONFIG_LVGL)\n\
#include <zephyr/device.h>\n\
#include <zephyr/drivers/display.h>\n\
#include <lvgl.h>\n\
#endif\n\
\n\
int main(void)\n\
{\n\
#if defined(CONFIG_LVGL)\n\
\tconst struct device *display = DEVICE_DT_GET(DT_CHOSEN(zephyr_display));\n\
\tif (!device_is_ready(display)) {\n\
\t\tprintk(\"Display is not ready\\n\");\n\
\t\treturn 0;\n\
\t}\n\
\tlv_obj_t *label = lv_label_create(lv_screen_active());\n\
\tlv_label_set_text(label, \"Hello from LVGL + SDL!\");\n\
\tlv_obj_center(label);\n\
\tlv_timer_handler();\n\
\tdisplay_blanking_off(display);\n\
#else\n\
\tprintk(\"Hello from %s\\n\", CONFIG_BOARD);\n\
#endif\n\
\twhile (1) {\n\
#if defined(CONFIG_LVGL)\n\
\t\tlv_timer_handler();\n\
#endif\n\
\t\tk_sleep(K_MSEC(10));\n\
\t}\n\
\treturn 0;\n\
}\n";

pub fn default_variant() -> Variant {
    Variant {
        name: "sim".into(),
        board: Some(DEFAULT_TARGET.into()),
        shield: None,
        build_dir: "build_sim".into(),
        origin: VariantOrigin::Declared,
    }
}

pub fn is_graphical_target(variant: &Variant) -> bool {
    variant
        .board
        .as_deref()
        .is_some_and(|target| target.split('/').next() == Some("native_sim"))
}

/// Deliberately narrow: these templates use native_sim's SDL devices and the
/// LVGL 9 API. Refuse an incompatible checkout instead of guessing symbols.
pub fn check_workspace(base: &Path) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("Zephyr native_sim + SDL preparation currently requires Linux.".into());
    }
    for (file, needles) in [
        (
            "boards/native/native_sim/native_sim.dts",
            &["sdl_dc:", "input_sdl_touch:"][..],
        ),
        (
            "modules/lvgl/Kconfig",
            &["config LV_Z_AUTO_INIT", "config LV_COLOR_DEPTH_32"][..],
        ),
        (
            "modules/lvgl/Kconfig.memory",
            &["config LV_Z_MEM_POOL_SIZE"][..],
        ),
        ("modules/lvgl/lvgl.c", &["lv_display_"][..]),
    ] {
        let path = base.join(file);
        let text = fs::read_to_string(&path).map_err(|err| {
            format!(
                "Cannot prepare LVGL + SDL: {}: {err}. Choose a compatible Zephyr workspace.",
                path.display()
            )
        })?;
        if !needles.iter().all(|needle| text.contains(needle)) {
            return Err(format!(
                "Cannot prepare LVGL + SDL: {} lacks the required native_sim/LVGL support. Use a compatible Zephyr 4.x checkout or configure the simulator manually.",
                path.display()
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Preparation {
    pub variant: Variant,
    pub original_name: Option<String>,
    /// Only written when the file previously declared no variants. Declared
    /// lists replace discovery, so its device targets must travel with sim.
    pub retained: Vec<Variant>,
    pub scaffold: Scaffold,
}

impl Preparation {
    pub fn new(
        root: &Path,
        app: &Path,
        variant: Variant,
        original_name: Option<String>,
        declared: &[Variant],
        mut discovered: Vec<Variant>,
    ) -> Result<Self, String> {
        validate_variant(&variant)?;
        let existing = if declared.is_empty() {
            &discovered
        } else {
            declared
        };
        for other in existing {
            if Some(&other.name) == original_name.as_ref() {
                continue;
            }
            if other.name == variant.name {
                return Err(format!(
                    "Variant '{}' already exists; choose a different name.",
                    variant.name
                ));
            }
            if directories_overlap(&other.build_dir, &variant.build_dir) {
                return Err(format!(
                    "Build directory '{}' overlaps variant '{}'; choose a separate directory.",
                    variant.build_dir, other.name
                ));
            }
        }
        if declared.is_empty() && !discovered.iter().any(|v| !v.is_simulator()) {
            let default_build = super::variants::build_path(root, Some(app), "build");
            if directories_overlap(&default_build, &variant.build_dir) {
                return Err(format!(
                    "The simulator must have its own directory, separate from the device's {default_build}/."
                ));
            }
            if variant.name == "hardware"
                || discovered
                    .iter()
                    .any(|v| v.name == "hardware" && Some(&v.name) != original_name.as_ref())
            {
                return Err("The name 'hardware' is reserved for the retained device variant; choose another name.".into());
            }
            // A host build may already occupy build/. The retained device
            // must not clean or flash that directory, even when this edit
            // moves the simulator somewhere else.
            let mut build_dir = default_build;
            for index in 0..=discovered.len() {
                if !discovered
                    .iter()
                    .any(|v| directories_overlap(&v.build_dir, &build_dir))
                {
                    break;
                }
                build_dir =
                    super::variants::build_path(root, Some(app), &format!("build_device_{index}"));
            }
            if directories_overlap(&build_dir, &variant.build_dir) {
                return Err(format!(
                    "The simulator directory overlaps the retained device's {build_dir}; choose another path."
                ));
            }
            discovered.insert(
                0,
                Variant {
                    name: "hardware".into(),
                    board: None,
                    shield: None,
                    build_dir,
                    origin: VariantOrigin::Declared,
                },
            );
        }
        let retained = if declared.is_empty() {
            discovered
                .into_iter()
                .filter(|v| Some(&v.name) != original_name.as_ref())
                .collect()
        } else {
            Vec::new()
        };
        let relative = app
            .strip_prefix(root)
            .map_err(|_| "The application must be inside the project.".to_string())?;
        let stem = variant.board.as_deref().unwrap().replace('/', "_");
        let mut files = vec![
            ScaffoldFile::new(relative.join(format!("boards/{stem}.conf")), CONF),
            ScaffoldFile::new(relative.join(format!("boards/{stem}.overlay")), OVERLAY),
        ];
        if crate::startup::is_empty_dir(root) {
            files.extend([
                ScaffoldFile::new("CMakeLists.txt", "cmake_minimum_required(VERSION 3.20.0)\nfind_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})\nproject(lvgl_app)\ntarget_sources(app PRIVATE src/main.c)\n"),
                ScaffoldFile::new("prj.conf", "# Shared options only. Target-specific options live under boards/.\n"),
                ScaffoldFile::new("src/main.c", MAIN),
            ]);
        }
        let plan = Self {
            variant,
            original_name,
            retained,
            scaffold: Scaffold {
                dirs: Vec::new(),
                files,
            },
        };
        plan.preflight(root)?;
        Ok(plan)
    }

    pub fn config_text(&self, text: &str) -> Result<String, String> {
        let mut updated = text.to_string();
        for variant in &self.retained {
            updated = config::upsert_variant(&updated, None, variant)?;
        }
        // An inferred simulator had no block to update before this prepare.
        let original = self
            .original_name
            .as_deref()
            .filter(|name| config::parse_variants(text).iter().any(|v| v.name == *name));
        config::upsert_variant(&updated, original, &self.variant)
    }

    pub fn preflight(&self, root: &Path) -> Result<(), String> {
        safe_target(root, Path::new(config::FILE_NAME))?;
        let build = safe_target(root, Path::new(&self.variant.build_dir))?;
        if build.exists() && !build.is_dir() {
            return Err(format!(
                "Build directory {} is not a directory; choose another path.",
                build.display()
            ));
        }
        if let Some(target) = crate::build::cached_target(root, &self.variant.build_dir)
            && Some(&target.board) != self.variant.board.as_ref()
        {
            return Err(format!(
                "Build directory '{}' is configured for {}; choose a separate simulator directory.",
                self.variant.build_dir, target.board
            ));
        }
        for file in &self.scaffold.files {
            let target = safe_target(root, &file.path)?;
            match fs::metadata(&target) {
                Ok(metadata) if !metadata.is_file() => {
                    return Err(format!(
                        "Cannot prepare simulator: {} is not a regular file. Resolve the conflict first.",
                        target.display()
                    ));
                }
                Err(err) if err.kind() != io::ErrorKind::NotFound => {
                    return Err(format!("Cannot inspect {}: {err}", target.display()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Creation uses create_new too: even a file created after review cannot
    /// be overwritten. A partial failure retains the pending preparation.
    pub fn create_files(&self, root: &Path) -> Result<(), String> {
        self.preflight(root)?;
        for file in &self.scaffold.files {
            let target = safe_target(root, &file.path)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
            }
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
            {
                Ok(mut output) => output.write_all(file.contents.as_bytes()).map_err(|err| {
                    format!(
                        "Cannot write {}: {err}. Check the file before retrying.",
                        target.display()
                    )
                })?,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists && target.is_file() => {}
                Err(err) => return Err(format!("Cannot create {}: {err}", target.display())),
            }
        }
        Ok(())
    }
}

/// A staged removal: only the declaration leaves `chiptui.toml`. Everything
/// the preparation wrote stays on disk --- files are the user's sources now,
/// not ChipTUI's to delete --- and the review names what stays behind.
#[derive(Debug, Clone)]
pub struct Removal {
    pub name: String,
    /// Fragment files the variant uses (project-relative), kept on disk.
    pub retained_files: Vec<PathBuf>,
    pub build_dir: String,
    /// True when removing this declaration empties the declared list while
    /// the target's files remain --- discovery may then bring it back.
    pub rediscovered: bool,
}

impl Removal {
    pub fn new(
        root: &Path,
        app: &Path,
        variant: &Variant,
        declared: &[Variant],
    ) -> Result<Self, String> {
        if variant.origin != VariantOrigin::Declared
            || !declared.iter().any(|v| v.name == variant.name)
        {
            return Err(format!(
                "'{}' is discovered, not declared; remove its build directory ({}/) instead.",
                variant.name, variant.build_dir
            ));
        }
        let relative = app
            .strip_prefix(root)
            .map_err(|_| "The application must be inside the project.".to_string())?;
        let stem = variant
            .board
            .as_deref()
            .unwrap_or(DEFAULT_TARGET)
            .replace('/', "_");
        let retained_files: Vec<PathBuf> = ["conf", "overlay"]
            .iter()
            .map(|ext| relative.join(format!("boards/{stem}.{ext}")))
            .filter(|path| root.join(path).is_file())
            .collect();
        let rediscovered = declared.len() == 1
            && (root.join(&variant.build_dir).is_dir() || !retained_files.is_empty());
        Ok(Self {
            name: variant.name.clone(),
            retained_files,
            build_dir: variant.build_dir.clone(),
            rediscovered,
        })
    }

    pub fn config_text(&self, text: &str) -> Result<String, String> {
        config::remove_variant(text, &self.name)
    }
}

fn safe_target(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let target = scaffold::resolve(root, relative).map_err(|err| err.to_string())?;
    let mut ancestor = root.to_path_buf();
    for part in relative.components() {
        ancestor.push(part);
        if fs::symlink_metadata(&ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "Cannot prepare simulator through symlink {}. Use files inside the project.",
                ancestor.display()
            ));
        }
    }
    Ok(target)
}

fn directories_overlap(a: &str, b: &str) -> bool {
    Path::new(a).starts_with(b) || Path::new(b).starts_with(a)
}

pub fn validate_variant(variant: &Variant) -> Result<(), String> {
    if variant.name.is_empty()
        || !variant
            .name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err("Use a variant name containing letters, numbers, '-' or '_'.".into());
    }
    if !variant
        .board
        .as_deref()
        .is_some_and(|target| TARGETS.contains(&target))
    {
        return Err(format!(
            "Choose {} or native_sim/native for LVGL + SDL.",
            DEFAULT_TARGET
        ));
    }
    let dir = Path::new(&variant.build_dir);
    if variant.build_dir.is_empty()
        || !dir.is_relative()
        || dir
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || !variant
            .build_dir
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '-'))
        || !dir
            .components()
            .any(|part| part.as_os_str().to_string_lossy().starts_with("build"))
    {
        return Err(
            "Use a project-relative build directory such as build_sim, app/build_sim or build/sim, without '..'."
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_refuses_escaping_and_non_native_targets() {
        let mut v = default_variant();
        v.build_dir = "app/build_sim".into();
        assert!(validate_variant(&v).is_ok());
        for dir in [
            "",
            "/tmp/build",
            "../build",
            "build/../src",
            "app/build/../../src",
            "src",
            "build\\sim",
        ] {
            v.build_dir = dir.into();
            assert!(validate_variant(&v).is_err(), "{dir}");
        }
        v.build_dir = "build_sim".into();
        v.board = Some("unit_testing".into());
        assert!(validate_variant(&v).is_err());
    }

    #[test]
    fn overlap_includes_nested_build_directories() {
        assert!(directories_overlap("build", "build/sim"));
        assert!(directories_overlap("build/sim", "build"));
        assert!(!directories_overlap("build", "build_sim"));
    }
}
