//! Configuration-time answers retained before `west build --pristine=always`.
//!
//! Read CMake's typed cache, never replay `west.command` from build_info.yml:
//! west writes that as an unquoted join of argv, so paths with spaces cannot
//! be reconstructed from it. Keep known Zephyr inputs and command-line cache
//! entries, not CMake's derived compiler checks and internal paths.

use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Entry {
    name: String,
    kind: String,
    value: String,
    command_line: bool,
}

fn entries(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut command_line = false;
    let mut found = Vec::new();
    for line in text.lines() {
        if let Some(comment) = line.strip_prefix("//") {
            command_line = comment.contains("specified on the command line");
            continue;
        }
        if let Some((left, value)) = line.split_once('=')
            && let Some((name, kind)) = left.split_once(':')
        {
            found.push(Entry {
                name: name.into(),
                kind: kind.into(),
                value: value.into(),
                command_line,
            });
        }
        command_line = false;
    }
    found
}

fn value<'a>(entries: &'a [Entry], name: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.value.as_str())
        .filter(|v| !v.is_empty())
}

/// The application cache under sysbuild, or the ordinary top-level cache.
pub fn application_cache(build: &Path) -> PathBuf {
    if let Some(domains) = super::domains::Domains::read(build) {
        return domains.domain_dir(&domains.default).join("CMakeCache.txt");
    }
    let cache = build.join("CMakeCache.txt");
    if cache.is_file() {
        cache
    } else {
        build.join("zephyr/CMakeCache.txt")
    }
}

/// Older/incomplete caches may omit the source; an explicit foreign source
/// is never offered as a configuration of this application.
pub fn belongs_to(root: &Path, app: Option<&Path>, dir: &str) -> bool {
    let cache = entries(&application_cache(&root.join(dir)));
    let source = value(&cache, "APP_DIR")
        .or_else(|| value(&cache, "APPLICATION_SOURCE_DIR"))
        .or_else(|| value(&cache, "CMAKE_HOME_DIRECTORY"));
    source.is_none_or(|source| {
        let source = root.join(source);
        let expected = app.unwrap_or(root);
        source.canonicalize().unwrap_or(source)
            == expected
                .canonicalize()
                .unwrap_or_else(|_| expected.to_path_buf())
    })
}

pub struct Rebuild {
    pub board: String,
    pub shield: Option<String>,
    pub sysbuild: bool,
    pub cmake_args: Vec<String>,
}

/// Resolve the selected directory before pristine can remove its cache.
pub fn rebuild(root: &Path, dir: &str) -> Result<Option<Rebuild>, String> {
    let build = root.join(dir);
    let sysbuild = build.join(super::domains::FILE_NAME).is_file();
    if sysbuild && super::domains::Domains::read(&build).is_none() {
        return Err(format!(
            "west rebuild cannot read {dir}/domains.yaml; repair or regenerate this build's sysbuild metadata first"
        ));
    }
    let app_cache = application_cache(&build);
    let app = entries(&app_cache);
    let Some(board) = value(&app, "CACHED_BOARD") else {
        if sysbuild {
            return Err(format!(
                "west rebuild cannot recover the application board from {}; regenerate this build's application cache first",
                app_cache.display()
            ));
        }
        return Ok(None);
    };
    let top = if sysbuild {
        entries(&build.join("CMakeCache.txt"))
    } else {
        Vec::new()
    };
    if sysbuild && top.is_empty() {
        return Err(format!(
            "west rebuild cannot recover configuration from {dir}/CMakeCache.txt; regenerate the sysbuild cache first"
        ));
    }
    let inputs = if sysbuild { &top } else { &app };
    let mut cmake_args = Vec::new();
    for entry in inputs {
        if matches!(
            entry.name.as_str(),
            "BOARD"
                | "CACHED_BOARD"
                | "SHIELD"
                | "APP_DIR"
                | "APPLICATION_SOURCE_DIR"
                | "APPLICATION_BINARY_DIR"
                | "WEST_PYTHON"
        ) {
            continue;
        }
        let known = matches!(
            entry.name.as_str(),
            "CONF_FILE"
                | "EXTRA_CONF_FILE"
                | "OVERLAY_CONFIG"
                | "DTC_OVERLAY_FILE"
                | "EXTRA_DTC_OVERLAY_FILE"
                | "SNIPPET"
                | "FILE_SUFFIX"
                | "BOARD_ROOT"
                | "DTS_ROOT"
                | "SOC_ROOT"
                | "ARCH_ROOT"
                | "ZEPHYR_EXTRA_MODULES"
                | "EXTRA_ZEPHYR_MODULES"
                | "ZEPHYR_MODULES"
                | "ZEPHYR_TOOLCHAIN_VARIANT"
                | "ZEPHYR_SDK_INSTALL_DIR"
                | "SB_CONF_FILE"
                | "SB_EXTRA_CONF_FILE"
        );
        if known
            || (entry.kind != "INTERNAL"
                && (entry.command_line
                    || entry.kind == "UNINITIALIZED"
                    || entry.name.starts_with("CONFIG_")
                    || entry.name.starts_with("SB_CONFIG_")))
        {
            cmake_args.push(format!("-D{}={}", entry.name, entry.value));
        }
    }
    Ok(Some(Rebuild {
        board: board.into(),
        shield: value(if sysbuild { &top } else { &app }, "SHIELD").map(str::to_string),
        sysbuild,
        cmake_args,
    }))
}
