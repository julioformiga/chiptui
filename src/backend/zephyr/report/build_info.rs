//! `<build>/build_info.yml` --- the build's own metadata, and the toolchain
//! version beside it.
//!
//! CMake writes this file on every configure, and it is where
//! `dashboard.py` reads the Build Summary's facts from
//! (`ZephyrDashboard.__init__`). The shape is narrow and machine-generated:
//! two-space indentation, `key: 'value'` scalars in single quotes, and
//! sequences as ` - 'value'` lines one space deeper than their key. Nothing
//! here is a general YAML reader --- no anchors, no flow collections, no
//! block scalars, no multi-document files --- because nothing writes those
//! into this file. It is the same bias as the config parsers
//! ([`crate::settings`]): one known shape, hand-rolled, no dependency.
//!
//! The reader itself is [`super::super::yaml`], shared with the module
//! manifests that carry the same shape.
//!
//! The compiler's *version* is the one Summary fact this file does not
//! carry. `dashboard.py` reads it out of `CMakeFiles/<ver>/CMakeCCompiler.cmake`
//! and so does [`toolchain_version`], from the same two `set(...)` lines.

use crate::backend::zephyr::yaml::{read_entries, scalar, sequence};

/// The Build Summary's facts, each independently optional: a build
/// interrupted before CMake finished, or a future CMake that renames a key,
/// must cost the pane a row rather than the whole tab.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    /// `cmake.application.source-dir` --- the application that was built.
    pub application: Option<String>,
    pub board: Option<String>,
    /// `cmake.board.qualifiers`, e.g. `esp32c3` --- the HWMv2 half of the
    /// target, shown beside the board name rather than folded into it.
    pub qualifiers: Option<String>,
    pub revision: Option<String>,
    /// `west.command` --- the invocation that produced this build, with the
    /// west executable's own path collapsed to `west` (see [`short_command`]).
    pub command: Option<String>,
    /// `west.topdir` --- the workspace root, which is also what
    /// `size_report` is passed as `--workspace`.
    pub topdir: Option<String>,
    pub west_version: Option<String>,
    pub zephyr_version: Option<String>,
    pub zephyr_base: Option<String>,
    pub toolchain: Option<String>,
    pub toolchain_path: Option<String>,
    /// The Kconfig fragments the user's own project contributed
    /// (`cmake.kconfig.user-files`) --- the short, interesting half of the
    /// configuration inputs, as opposed to every board and module fragment.
    pub kconfig_files: Vec<String>,
    /// `cmake.devicetree.user-files` --- the overlays the project added.
    pub overlay_files: Vec<String>,
}

/// Reads `build_info.yml`. Always returns a value: an unreadable or foreign
/// file simply answers every field `None`, which the Summary tab renders as
/// a row of dashes rather than an error --- the file is a build product, and
/// its absence already shows up as "no build directory".
pub fn parse(text: &str) -> BuildInfo {
    let entries = read_entries(text);
    BuildInfo {
        application: scalar(&entries, "cmake.application.source-dir"),
        board: scalar(&entries, "cmake.board.name"),
        qualifiers: scalar(&entries, "cmake.board.qualifiers"),
        revision: scalar(&entries, "cmake.board.revision"),
        command: scalar(&entries, "west.command")
            .as_deref()
            .map(short_command),
        topdir: scalar(&entries, "west.topdir"),
        west_version: scalar(&entries, "west.version"),
        zephyr_version: scalar(&entries, "cmake.zephyr.version"),
        zephyr_base: scalar(&entries, "cmake.zephyr.zephyr-base"),
        toolchain: scalar(&entries, "cmake.toolchain.name"),
        toolchain_path: scalar(&entries, "cmake.toolchain.path"),
        kconfig_files: sequence(&entries, "cmake.kconfig.user-files"),
        overlay_files: sequence(&entries, "cmake.devicetree.user-files"),
    }
}

/// The west invocation with the executable's path reduced to its name.
///
/// `build_info.yml` records the absolute path west was launched from
/// (`/mnt/dev/zephyr/.venv/bin/west build ...`), which is both long and
/// uninteresting --- the workspace it belongs to is already the pane's
/// subject. `dashboard.py` collapses it with `re.sub(r'\S*west', 'west', …)`;
/// this does the same to the *first* token only, so a `--shield`
/// argument that happens to contain "west" is untouched.
fn short_command(command: &str) -> String {
    let command = command.trim();
    let Some((program, rest)) = command.split_once(char::is_whitespace) else {
        return trim_program(command).to_string();
    };
    format!("{} {rest}", trim_program(program))
}

fn trim_program(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// One key's value, flattened out of the nesting.
/// The C compiler's id and version, read from a
/// `CMakeFiles/<ver>/CMakeCCompiler.cmake` file's text, e.g. `GNU 14.3.0`.
///
/// The two `set(...)` lines are matched with the space after the variable
/// name included, which is what keeps `CMAKE_C_COMPILER_VERSION_INTERNAL`
/// --- written on the very next line, and usually empty --- from answering
/// for `CMAKE_C_COMPILER_VERSION`. `dashboard.py`'s own regexes anchor the
/// same way.
pub fn toolchain_version(text: &str) -> Option<String> {
    let id = cmake_set(text, "CMAKE_C_COMPILER_ID");
    let version = cmake_set(text, "CMAKE_C_COMPILER_VERSION");
    match (id, version) {
        (Some(id), Some(version)) => Some(format!("{id} {version}")),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

fn cmake_set(text: &str, variable: &str) -> Option<String> {
    let needle = format!("set({variable} \"");
    text.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(needle.as_str())?;
        let value = rest.strip_suffix("\")")?;
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cut from a real `build_info.yml` (Zephyr 4.4.99, xiao_esp32c3 with a
    /// shield). It carries every shape the reader must survive: nested maps,
    /// sequences indented one space past their key, an empty scalar
    /// (`revision`), a sibling key that follows a sequence (`qualifiers`
    /// after `path`), and a top-level scalar between two maps (`version`).
    const SAMPLE: &str = "\
cmake:
  application:
    configuration-dir: '/home/j/projects/round-display'
    source-dir: '/home/j/projects/round-display'
  board:
    name: 'xiao_esp32c3'
    path:
     - '/mnt/dev/zephyr/zephyr/boards/seeed/xiao_esp32c3'
    qualifiers: 'esp32c3'
    revision: ''
  devicetree:
    files:
     - '/mnt/dev/zephyr/zephyr/boards/seeed/xiao_esp32c3/xiao_esp32c3.dts'
     - '/mnt/dev/zephyr/zephyr/boards/shields/seeed_xiao_round_display/x.overlay'
    user-files:
     - '/home/j/projects/round-display/boards/xiao_esp32c3.overlay'
  kconfig:
    files:
     - '/mnt/dev/zephyr/zephyr/boards/seeed/xiao_esp32c3/xiao_esp32c3_defconfig'
     - '/home/j/projects/round-display/prj.conf'
    user-files:
     - '/home/j/projects/round-display/prj.conf'
     - '/home/j/projects/round-display/boards/xiao_esp32c3.conf'
  toolchain:
    name: 'zephyr'
    path: '/mnt/windows/zephyr/zephyr-sdk-1.0.1'
  zephyr:
    version: '4.4.99'
    zephyr-base: '/mnt/dev/zephyr/zephyr'
version: '0.1.0'
west:
  command: '/mnt/dev/zephyr/.venv/bin/west build --pristine=always -b xiao_esp32c3'
  topdir: '/mnt/dev/zephyr'
  version: '1.5.0'
";

    #[test]
    fn the_summary_facts_are_read() {
        let info = parse(SAMPLE);
        assert_eq!(info.board.as_deref(), Some("xiao_esp32c3"));
        assert_eq!(info.qualifiers.as_deref(), Some("esp32c3"));
        assert_eq!(
            info.application.as_deref(),
            Some("/home/j/projects/round-display")
        );
        assert_eq!(info.zephyr_version.as_deref(), Some("4.4.99"));
        assert_eq!(info.zephyr_base.as_deref(), Some("/mnt/dev/zephyr/zephyr"));
        assert_eq!(info.toolchain.as_deref(), Some("zephyr"));
        assert_eq!(
            info.toolchain_path.as_deref(),
            Some("/mnt/windows/zephyr/zephyr-sdk-1.0.1")
        );
        assert_eq!(info.topdir.as_deref(), Some("/mnt/dev/zephyr"));
        assert_eq!(info.west_version.as_deref(), Some("1.5.0"));
    }

    /// A key's path is its whole nesting: `cmake.board.name` and
    /// `cmake.toolchain.name` are different keys that a last-segment match
    /// would confuse, and `version` appears at three different depths.
    #[test]
    fn keys_are_addressed_by_their_whole_path() {
        let entries = read_entries(SAMPLE);
        assert_eq!(
            scalar(&entries, "cmake.board.name").as_deref(),
            Some("xiao_esp32c3")
        );
        assert_eq!(
            scalar(&entries, "cmake.toolchain.name").as_deref(),
            Some("zephyr")
        );
        assert_eq!(scalar(&entries, "version").as_deref(), Some("0.1.0"));
        assert_eq!(scalar(&entries, "west.version").as_deref(), Some("1.5.0"));
        assert_eq!(
            scalar(&entries, "cmake.zephyr.version").as_deref(),
            Some("4.4.99")
        );
    }

    /// Sequences sit one space deeper than their key, and the key that
    /// follows one has to close it --- `qualifiers` comes right after
    /// `path`'s single item and must not be swallowed into it.
    #[test]
    fn sequences_are_collected_and_closed_by_the_next_key() {
        let info = parse(SAMPLE);
        assert_eq!(
            info.kconfig_files,
            vec![
                "/home/j/projects/round-display/prj.conf".to_string(),
                "/home/j/projects/round-display/boards/xiao_esp32c3.conf".to_string(),
            ]
        );
        assert_eq!(
            info.overlay_files,
            vec!["/home/j/projects/round-display/boards/xiao_esp32c3.overlay".to_string()]
        );
        // The sibling after the sequence still reads.
        assert_eq!(info.qualifiers.as_deref(), Some("esp32c3"));
    }

    /// `revision: ''` means "this board has no revision", not "the revision
    /// is the empty string": the row is dropped rather than drawn blank.
    #[test]
    fn an_empty_scalar_reads_as_absent() {
        assert_eq!(parse(SAMPLE).revision, None);
    }

    /// The recorded invocation names west by its absolute venv path; the
    /// pane shows the command, not where the binary lives.
    #[test]
    fn the_west_command_loses_only_its_program_path() {
        let info = parse(SAMPLE);
        assert_eq!(
            info.command.as_deref(),
            Some("west build --pristine=always -b xiao_esp32c3")
        );
    }

    /// Only the first token is collapsed --- an argument that happens to
    /// contain a path is the user's own text and stays whole.
    #[test]
    fn only_the_program_token_is_shortened() {
        let text = "west:\n  command: '/a/b/west build -- -DX=/opt/west/thing'\n";
        assert_eq!(
            parse(text).command.as_deref(),
            Some("west build -- -DX=/opt/west/thing")
        );
    }

    #[test]
    fn a_missing_or_foreign_file_answers_every_field_none() {
        for text in ["", "   \n\n", "not: yaml: at: all\n", "# only a comment\n"] {
            let info = parse(text);
            assert_eq!(info.board, None, "board from {text:?}");
            assert_eq!(info.application, None);
            assert!(info.kconfig_files.is_empty());
        }
    }

    /// A build killed mid-write leaves the file cut off; every key that did
    /// land must still read.
    #[test]
    fn a_truncated_file_still_yields_what_it_holds() {
        let cut = SAMPLE.split_once("  devicetree:").expect("has the key").0;
        let info = parse(cut);
        assert_eq!(info.board.as_deref(), Some("xiao_esp32c3"));
        assert_eq!(info.zephyr_version, None);
        assert_eq!(info.command, None);
    }

    const CMAKE_COMPILER: &str = "\
set(CMAKE_C_COMPILER \"/opt/sdk/gnu/riscv64-zephyr-elf/bin/riscv64-zephyr-elf-gcc\")
set(CMAKE_C_COMPILER_ARG1 \"\")
set(CMAKE_C_COMPILER_ID \"GNU\")
set(CMAKE_C_COMPILER_VERSION \"14.3.0\")
set(CMAKE_C_COMPILER_VERSION_INTERNAL \"\")
set(CMAKE_C_COMPILER_ID_RUN 1)
";

    #[test]
    fn the_compiler_id_and_version_are_read() {
        assert_eq!(
            toolchain_version(CMAKE_COMPILER).as_deref(),
            Some("GNU 14.3.0")
        );
    }

    /// `CMAKE_C_COMPILER_VERSION_INTERNAL` sits on the line right after
    /// `CMAKE_C_COMPILER_VERSION` and is usually empty: a prefix match would
    /// answer the version with nothing.
    #[test]
    fn the_internal_version_never_answers_for_the_real_one() {
        let reordered = "\
set(CMAKE_C_COMPILER_VERSION_INTERNAL \"\")
set(CMAKE_C_COMPILER_ID \"GNU\")
set(CMAKE_C_COMPILER_VERSION \"14.3.0\")
";
        assert_eq!(
            toolchain_version(reordered).as_deref(),
            Some("GNU 14.3.0"),
            "the internal variable must not shadow the real one"
        );
    }

    #[test]
    fn a_compiler_file_with_neither_line_answers_none() {
        assert_eq!(
            toolchain_version("set(CMAKE_SYSTEM_NAME \"Generic\")"),
            None
        );
    }
}
