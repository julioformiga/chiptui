//! The installation sequence, as commands.
//!
//! One step per command in the Zephyr getting-started guide, built here and
//! nowhere else (`SPEC.md` §23: centralized command construction is the
//! mitigation for upstream CLI drift, the same rule
//! [`crate::backend::zephyr::commands`] follows for `west`).
//!
//! Three things about this sequence are not obvious from the guide:
//!
//! * **`source .venv/bin/activate` has no equivalent here.** Activation is a
//!   shell mutation, and ChipTUI runs no shell (`AGENTS.md` §5). Every venv
//!   command is invoked by its absolute path instead --- `<root>/.venv/bin/west`
//!   --- exactly the way [`crate::backend::zephyr::workspace`] already resolves
//!   the workspace's west: a venv console script embeds its interpreter, so
//!   there is nothing to activate.
//! * **`python3` off `PATH` does not honour `pyenv local`.** That only works
//!   through pyenv's shims, which may or may not be on this `PATH`. So
//!   [`Step::PyenvRoot`] asks pyenv where it keeps its versions and the venv
//!   is created by the absolute interpreter underneath it. `pyenv local` still
//!   runs --- it is what makes the checkout self-describing for anyone who
//!   opens a shell in it later --- but nothing in this sequence depends on it.
//! * **`west sdk install` chooses its destination with `-b`, never `-d`.**
//!   `-d/--install-dir` is the *full final path of the SDK directory*, which
//!   the extracted `zephyr-sdk-<version>` is renamed to --- it is not "install
//!   into this directory", and it silently overrides `-b`. Passing `-d ..`
//!   extracts several GB inside the git checkout, hands `run_setup` the
//!   literal path `..`, and so runs `../setup.sh` from the checkout --- a file
//!   that does not exist. West dies there, *after* moving a bundle into place
//!   but *before* any toolchain is downloaded or the SDK is registered, so it
//!   looks half-installed and `west sdk list` still reports nothing.
//!   `-b/--install-base` is the flag that means what `-d` looked like it
//!   meant: it produces `<BASE>/zephyr-sdk-<version>`.
//! * **The SDK step's working directory is not about the destination.** It
//!   runs in `<root>/zephyr` for the same reason the guide's `cd` does: west
//!   locates the workspace from the cwd and reads `${ZEPHYR_BASE}/SDK_VERSION`
//!   for the version to fetch. The destination is `-b`'s job, and it is passed
//!   absolute so nothing downstream depends on the cwd.
//!
//! Resumption ([`Step::already_done`]) reads the filesystem, never a record of
//! what this app did before: an installation interrupted by a crash, a reboot
//! or a different tool resumes the same way.

use std::path::{Path, PathBuf};

use crate::process::Command;

/// The manifest `west init` clones. The upstream repository, pinned to no
/// revision --- `west init` without `--mr` takes the default branch, which is
/// what the guide's copy-paste line does.
pub const MANIFEST_URL: &str = "https://github.com/zephyrproject-rtos/zephyr";

/// The venv the guide creates, at the workspace root.
pub const VENV_DIR: &str = ".venv";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `pyenv install --list` --- which 3.12 to pin. A query: its answer
    /// feeds the steps below rather than changing anything on disk.
    ResolvePython,
    /// `pyenv root` --- where pyenv keeps its interpreters. Also a query.
    PyenvRoot,
    /// `pyenv install --skip-existing <version>` --- builds the interpreter
    /// unless pyenv already has it. Slow the first time (it compiles CPython).
    PyenvInstall,
    /// `pyenv local <version>` --- writes `.python-version` in the workspace
    /// root, so the pin is visible to anything else that opens the folder.
    PyenvLocal,
    /// `<pyenv>/versions/<version>/bin/python -m venv .venv`.
    Venv,
    /// `.venv/bin/pip install west`.
    PipWest,
    /// `.venv/bin/west init -m <manifest> .`
    WestInit,
    /// `.venv/bin/west update` --- the long one: clones every module the
    /// manifest names.
    WestUpdate,
    /// `.venv/bin/west packages pip --install`.
    WestPackages,
    /// `.venv/bin/west zephyr-export` --- registers the Zephyr CMake package
    /// for this user, which is what lets an application outside the workspace
    /// find it.
    WestExport,
    /// `.venv/bin/west sdk install -b <root> -t NAME...`, run in the manifest
    /// checkout (see the module docs for why that is *not* what puts the
    /// bundle in the workspace root --- `-b` is).
    SdkInstall,
    /// `.venv/bin/west sdk list` --- the confirmation that the bundle
    /// registered, run *after* the install because that is the only time it
    /// can work: `list` reads the CMake user package registry
    /// (`~/.cmake/packages/Zephyr-sdk/`) and dies with
    /// `FATAL ERROR: No Zephyr SDK installed.` when it is empty. It reports
    /// and nothing consumes its output, so it is [`Self::optional`].
    SdkList,
}

impl Step {
    /// The sequence, in the order it runs.
    pub const ALL: &'static [Step] = &[
        Step::ResolvePython,
        Step::PyenvRoot,
        Step::PyenvInstall,
        Step::PyenvLocal,
        Step::Venv,
        Step::PipWest,
        Step::WestInit,
        Step::WestUpdate,
        Step::WestPackages,
        Step::WestExport,
        Step::SdkInstall,
        Step::SdkList,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ResolvePython => "Find Python 3.12",
            Self::PyenvRoot => "Locate pyenv",
            Self::PyenvInstall => "Install Python",
            Self::PyenvLocal => "Pin Python",
            Self::Venv => "Create the venv",
            Self::PipWest => "Install west",
            Self::WestInit => "Init the workspace",
            Self::WestUpdate => "Update the workspace",
            Self::WestPackages => "Install packages",
            Self::WestExport => "Export to CMake",
            Self::SdkInstall => "Install the SDK",
            Self::SdkList => "Verify the SDK",
        }
    }

    /// Whether failing this step leaves the installation usable, so the
    /// sequence marks it and carries on instead of stopping.
    ///
    /// Only the SDK confirmation qualifies: it reports on what the step
    /// before it did, and nothing downstream reads its output. Every other
    /// step either changes the workspace or answers a question a later
    /// command needs --- [`Self::ResolvePython`] and [`Self::PyenvRoot`]
    /// especially, where stopping with their own error beats dying later on
    /// a command that could not be built.
    pub const fn optional(self) -> bool {
        matches!(self, Self::SdkList)
    }

    /// The two steps the user may decline: the SDK bundle is several GB, and
    /// a machine may already have one --- its confirmation goes with it.
    pub const fn belongs_to_sdk(self) -> bool {
        matches!(self, Self::SdkList | Self::SdkInstall)
    }

    /// The working directory this step runs in: the workspace root for
    /// everything except the SDK install, which runs in the manifest
    /// checkout so west can resolve the workspace and read
    /// `${ZEPHYR_BASE}/SDK_VERSION` --- the guide's own `cd`. Where the
    /// bundle *lands* is `-b`'s answer, not this one.
    pub fn cwd(self, root: &Path) -> PathBuf {
        if self == Self::SdkInstall {
            root.join("zephyr")
        } else {
            root.to_path_buf()
        }
    }

    /// The command, or `None` when an earlier query has not answered yet
    /// (a step that needs the Python version before the version is known).
    pub fn command(self, ctx: &Context<'_>) -> Option<Command> {
        let command = match self {
            Self::ResolvePython => Command::new(ctx.pyenv).arg("install").arg("--list"),
            Self::PyenvRoot => Command::new(ctx.pyenv).arg("root"),
            Self::PyenvInstall => Command::new(ctx.pyenv)
                .arg("install")
                .arg("--skip-existing")
                .arg(ctx.python?),
            Self::PyenvLocal => Command::new(ctx.pyenv).arg("local").arg(ctx.python?),
            Self::Venv => Command::new(ctx.interpreter()?.to_string_lossy().into_owned())
                .arg("-m")
                .arg("venv")
                .arg(VENV_DIR),
            Self::PipWest => Command::new(venv_bin(ctx.root, "pip"))
                .arg("install")
                .arg("west"),
            Self::WestInit => west(ctx.root)
                .arg("init")
                .arg("-m")
                .arg(MANIFEST_URL)
                .arg("."),
            Self::WestUpdate => west(ctx.root).arg("update"),
            Self::WestPackages => west(ctx.root).arg("packages").arg("pip").arg("--install"),
            Self::WestExport => west(ctx.root).arg("zephyr-export"),
            Self::SdkList => west(ctx.root).arg("sdk").arg("list"),
            Self::SdkInstall => {
                // `-b`, absolute: `-d` renames the SDK directory to the path
                // given and breaks `setup.sh` (module docs), and a relative
                // base would reach `setup.sh` as a cwd-dependent path.
                let command = west(ctx.root)
                    .arg("sdk")
                    .arg("install")
                    .arg("-b")
                    .arg(ctx.root.display().to_string());
                // One `-t` carrying every name, and last on the line.
                // `west sdk install` declares it `nargs="+"`, not
                // `action="append"`: a repeated `-t` makes argparse
                // *overwrite*, silently keeping only the final name, and the
                // greedy `+` would swallow any option that followed.
                //
                // `-t` is also the spelling to use rather than the long
                // form: current Zephyr calls it `--gnu-toolchains` and keeps
                // `--toolchains` only as a hidden deprecated alias, while
                // older Zephyr knows `--toolchains` alone. The short flag
                // means the same thing in both.
                if ctx.toolchains.is_empty() {
                    command
                } else {
                    command.arg("-t").args(ctx.toolchains.iter().cloned())
                }
            }
        };
        Some(command.current_dir(self.cwd(ctx.root)))
    }

    /// Whether the workspace already carries this step's result, read off
    /// the filesystem so an interrupted installation resumes wherever it
    /// actually stopped.
    ///
    /// [`Step::WestPackages`] and [`Step::WestExport`] deliberately never
    /// qualify: neither leaves a marker in the workspace (`zephyr-export`
    /// writes into `~/.cmake/packages`, `packages pip --install` into the
    /// venv), and both are idempotent and quick. Re-running them costs
    /// seconds; skipping them wrongly costs a build that cannot find Zephyr.
    /// The queries never qualify either --- their answers live in memory.
    pub fn already_done(self, root: &Path) -> bool {
        match self {
            Self::ResolvePython
            | Self::PyenvRoot
            | Self::WestPackages
            | Self::WestExport
            | Self::SdkList => false,
            Self::PyenvInstall | Self::PyenvLocal => root.join(".python-version").is_file(),
            // The venv counts as built only once it carries west: a venv
            // whose `pip install` was interrupted is not a finished step.
            Self::Venv | Self::PipWest => {
                crate::backend::executable_at(&venv_bin_path(root, "west"))
            }
            Self::WestInit => root.join(".west").is_dir(),
            // `west init` creates `.west/` before it has a checkout; the
            // manifest's own VERSION file is what `west update` leaves.
            Self::WestUpdate => root.join("zephyr").join("VERSION").is_file(),
            Self::SdkInstall => installed_sdk(root).is_some(),
        }
    }
}

/// What the steps need to know that only the run itself can tell them.
pub struct Context<'a> {
    pub root: &'a Path,
    /// The `pyenv` program --- overridable, so tests point it at a fixture.
    pub pyenv: &'a str,
    /// The resolved 3.12.x, once [`Step::ResolvePython`] answered.
    pub python: Option<&'a str>,
    /// Where pyenv keeps its interpreters, once [`Step::PyenvRoot`] answered.
    pub pyenv_root: Option<&'a Path>,
    /// The toolchains the SDK step should install --- the ones still
    /// *missing*, not everything the user ever picked: with the bundle
    /// already unpacked, west skips the download and runs `setup.sh -t` per
    /// name, so asking only for what is absent is the difference between
    /// adding one toolchain and re-running the lot. Owned because it is a
    /// computed delta rather than a field to borrow.
    pub toolchains: Vec<String>,
}

impl Context<'_> {
    /// The interpreter the venv is built from: pyenv's own, by absolute
    /// path (see the module docs on why the shim is not enough).
    fn interpreter(&self) -> Option<PathBuf> {
        Some(
            self.pyenv_root?
                .join("versions")
                .join(self.python?)
                .join("bin")
                .join("python"),
        )
    }
}

fn venv_bin_path(root: &Path, program: &str) -> PathBuf {
    root.join(VENV_DIR).join("bin").join(program)
}

fn venv_bin(root: &Path, program: &str) -> String {
    venv_bin_path(root, program).to_string_lossy().into_owned()
}

fn west(root: &Path) -> Command {
    Command::new(venv_bin(root, "west"))
}

/// The SDK bundle in `root` that *this workspace* needs, if it is there.
///
/// Version-aware on purpose. The workspace pins its SDK version in the
/// manifest checkout's `SDK_VERSION` (which is what `west sdk install`
/// defaults its `--version` to), and the bundle names itself with the
/// version it is --- so a `zephyr-sdk-0.16.0` sitting beside a workspace
/// that asks for `0.17.0` is not an answer, and the step that installs it
/// stays pending. That is what makes a Zephyr version bump re-run the SDK
/// step instead of silently building against the wrong toolchains.
///
/// Before `west update` there is no `SDK_VERSION` to read, and then any
/// bundle counts --- the resumption path has nothing better to go on.
pub fn installed_sdk(root: &Path) -> Option<PathBuf> {
    if let Some(wanted) = sdk_version(root) {
        let exact = root.join(format!("zephyr-sdk-{wanted}"));
        return exact.is_dir().then_some(exact);
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("zephyr-sdk-")
        })
        .map(|entry| entry.path())
        .collect();
    found.sort();
    // Newest by name when a machine carries more than one and nothing pins
    // a version --- any answer here beats none.
    found.pop()
}

/// The toolchains actually installed inside `sdk`.
///
/// A bundle ships the *list* of what it offers (`sdk_gnu_toolchains`) but
/// unpacks only what was asked for, so the directories are the truth. SDK
/// 1.0.0 and later keep them under `gnu/`; older bundles put them straight
/// in the SDK root beside `cmake/` and `hosttools/`, which is why the root
/// case filters by name.
pub fn installed_toolchains(sdk: &Path) -> Vec<String> {
    let gnu = sdk.join("gnu");
    let dir = if gnu.is_dir() { gnu } else { sdk.to_path_buf() };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| is_toolchain_name(name))
        .collect();
    found.sort();
    found
}

/// Whether a directory name is a Zephyr SDK toolchain. The stable part of
/// every name across targets and SDK releases.
fn is_toolchain_name(name: &str) -> bool {
    name.contains("zephyr-elf") || name.contains("zephyr-eabi")
}

/// The GNU toolchains `west sdk install -t` accepts.
///
/// This list is *curated*, not discovered, because nothing can discover it:
/// `west sdk list` reads the CMake user package registry and answers only
/// once an SDK is installed, and the valid names come from the GitHub
/// release assets that `west sdk install` itself fetches --- there is no
/// command that enumerates them beforehand.
///
/// The names below are the 35 in SDK 1.0.1's own `sdk_gnu_toolchains`. Going
/// stale is an acceptable failure mode: `west sdk install` validates every
/// name and, on a miss, dies printing the list it *does* accept --- which is
/// a better answer than anything guessed here. Extend or replace when the
/// SDK ships new targets.
pub const TOOLCHAINS: &[&str] = &[
    "aarch64-zephyr-elf",
    "arc64-zephyr-elf",
    "arc-zephyr-elf",
    "arm-zephyr-eabi",
    "microblazeel-zephyr-elf",
    "mips-zephyr-elf",
    "or1k-zephyr-elf",
    "riscv64-zephyr-elf",
    "rx-zephyr-elf",
    "sparc-zephyr-elf",
    "x86_64-zephyr-elf",
    "xtensa-amd_acp_6_0_adsp_zephyr-elf",
    "xtensa-amd_acp_7_0_adsp_zephyr-elf",
    "xtensa-amd_acp_7_3_adsp_zephyr-elf",
    "xtensa-dc233c_zephyr-elf",
    "xtensa-espressif_esp32_zephyr-elf",
    "xtensa-espressif_esp32s2_zephyr-elf",
    "xtensa-espressif_esp32s3_zephyr-elf",
    "xtensa-intel_ace15_mtpm_zephyr-elf",
    "xtensa-intel_ace30_ptl_zephyr-elf",
    "xtensa-intel_ace40_zephyr-elf",
    "xtensa-intel_tgl_adsp_zephyr-elf",
    "xtensa-mtk_mt8195_adsp_zephyr-elf",
    "xtensa-mtk_mt818x_adsp_zephyr-elf",
    "xtensa-mtk_mt8196_adsp_zephyr-elf",
    "xtensa-mtk_mt8365_adsp_zephyr-elf",
    "xtensa-nxp_imx_adsp_zephyr-elf",
    "xtensa-nxp_imx8m_adsp_zephyr-elf",
    "xtensa-nxp_imx8ulp_adsp_zephyr-elf",
    "xtensa-nxp_rt500_adsp_zephyr-elf",
    "xtensa-nxp_rt600_adsp_zephyr-elf",
    "xtensa-nxp_rt700_hifi1_zephyr-elf",
    "xtensa-nxp_rt700_hifi4_zephyr-elf",
    "xtensa-sample_controller_zephyr-elf",
    "xtensa-sample_controller32_zephyr-elf",
];

/// The SDK version this workspace is meant to pair with, from the manifest
/// checkout's `SDK_VERSION` --- the same file `west sdk install` defaults
/// its `--version` to. Present as soon as `west update` finishes, and read
/// from the file rather than asked of a subprocess, like every other version
/// this app reports.
pub fn sdk_version(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("zephyr").join("SDK_VERSION")).ok()?;
    let version = text.lines().next()?.trim().to_string();
    (!version.is_empty()).then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(root: &'a Path, toolchains: &[String]) -> Context<'a> {
        Context {
            root,
            pyenv: "pyenv",
            python: Some("3.12.13"),
            pyenv_root: Some(Path::new("/home/dev/.pyenv")),
            toolchains: toolchains.to_vec(),
        }
    }

    #[test]
    fn the_sequence_matches_the_getting_started_guide() {
        let root = Path::new("/w/zephyr");
        let none: [String; 0] = [];
        let ctx = context(root, &none);
        let rendered: Vec<String> = Step::ALL
            .iter()
            .map(|step| step.command(&ctx).expect("command").to_string())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "pyenv install --list",
                "pyenv root",
                "pyenv install --skip-existing 3.12.13",
                "pyenv local 3.12.13",
                "python -m venv .venv",
                "pip install west",
                "west init -m https://github.com/zephyrproject-rtos/zephyr .",
                "west update",
                "west packages pip --install",
                "west zephyr-export",
                "west sdk install -b /w/zephyr",
                "west sdk list",
            ]
        );
    }

    #[test]
    fn venv_commands_are_absolute_paths_never_an_activation() {
        let root = Path::new("/w/zephyr");
        let none: [String; 0] = [];
        let ctx = context(root, &none);
        // `Display` shows the program's file name; the command carries the
        // whole path, which is what makes activation unnecessary.
        let command = Step::WestUpdate.command(&ctx).expect("command");
        assert_eq!(command.program(), "/w/zephyr/.venv/bin/west");
        let command = Step::Venv.command(&ctx).expect("command");
        assert_eq!(
            command.program(),
            "/home/dev/.pyenv/versions/3.12.13/bin/python"
        );
    }

    #[test]
    fn a_step_waiting_on_a_query_has_no_command_yet() {
        let root = Path::new("/w/zephyr");
        let none: [String; 0] = [];
        let ctx = Context {
            python: None,
            pyenv_root: None,
            ..context(root, &none)
        };
        assert!(Step::PyenvInstall.command(&ctx).is_none());
        assert!(Step::Venv.command(&ctx).is_none());
        // The queries themselves never wait on anything.
        assert!(Step::ResolvePython.command(&ctx).is_some());
        assert!(Step::PyenvRoot.command(&ctx).is_some());
    }

    #[test]
    fn the_sdk_installs_into_the_workspace_with_install_base() {
        let root = Path::new("/w/zephyr");
        let picked = [
            "arm-zephyr-eabi".to_string(),
            "riscv64-zephyr-elf".to_string(),
        ];
        let ctx = context(root, &picked);
        let command = Step::SdkInstall.command(&ctx).expect("command");
        // `-b BASE` produces `BASE/zephyr-sdk-<version>`. One `-t`, every
        // name after it, `-t` last: the option is `nargs="+"`, so a repeated
        // flag would keep only the final name and the greedy `+` would eat a
        // later option --- which is why `-b` has to come before it.
        assert_eq!(
            command.to_string(),
            "west sdk install -b /w/zephyr -t arm-zephyr-eabi riscv64-zephyr-elf"
        );
        // The cwd is the manifest checkout, but only so west can resolve the
        // workspace and read SDK_VERSION --- not to make `..` mean anything.
        assert_eq!(
            command.cwd().map(PathBuf::as_path),
            Some(Path::new("/w/zephyr/zephyr"))
        );
        assert_eq!(
            Step::WestUpdate
                .command(&ctx)
                .expect("command")
                .cwd()
                .map(PathBuf::as_path),
            Some(root)
        );
    }

    #[test]
    fn the_sdk_install_never_uses_install_dir() {
        // Regression guard. `-d/--install-dir` is the SDK directory's final
        // *name*, not a destination to install into, and it overrides `-b`.
        // `-d ..` extracts gigabytes inside the git checkout and then runs
        // `../setup.sh` from there --- a file that does not exist --- so west
        // dies after moving a bundle into place but before downloading a
        // single toolchain or registering anything.
        let root = Path::new("/w/zephyr");
        let none: [String; 0] = [];
        let ctx = context(root, &none);
        let rendered = Step::SdkInstall.command(&ctx).expect("command").to_string();
        assert!(
            !rendered.contains(" -d "),
            "the SDK step must never pass --install-dir: {rendered}"
        );
        assert!(rendered.contains(" -b /w/zephyr"), "{rendered}");
    }

    #[test]
    fn the_installed_sdk_must_match_the_version_the_workspace_pins() {
        let dir = std::env::temp_dir().join(format!("chiptui-sdkmatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("zephyr")).unwrap();

        // No pin readable yet (before `west update`): any bundle answers.
        std::fs::create_dir_all(dir.join("zephyr-sdk-0.16.0")).unwrap();
        assert_eq!(installed_sdk(&dir), Some(dir.join("zephyr-sdk-0.16.0")));

        // Pinned: a bundle of another version is not this workspace's SDK,
        // so the step that installs it stays pending.
        std::fs::write(dir.join("zephyr/SDK_VERSION"), "0.17.0\n").unwrap();
        assert_eq!(installed_sdk(&dir), None);
        assert!(!Step::SdkInstall.already_done(&dir));

        std::fs::create_dir_all(dir.join("zephyr-sdk-0.17.0")).unwrap();
        assert_eq!(installed_sdk(&dir), Some(dir.join("zephyr-sdk-0.17.0")));
        assert!(Step::SdkInstall.already_done(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installed_toolchains_reads_the_directories_not_the_offered_list() {
        let dir = std::env::temp_dir().join(format!("chiptui-tcs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // SDK >= 1.0.0: toolchains under gnu/. The bundle *offers* 35 in
        // `sdk_gnu_toolchains`, but only what was unpacked is installed ---
        // the file would be the wrong thing to read.
        std::fs::create_dir_all(dir.join("gnu/arm-zephyr-eabi")).unwrap();
        std::fs::create_dir_all(dir.join("gnu/riscv64-zephyr-elf")).unwrap();
        std::fs::write(dir.join("sdk_gnu_toolchains"), "aarch64-zephyr-elf\n").unwrap();
        std::fs::create_dir_all(dir.join("hosttools")).unwrap();
        assert_eq!(
            installed_toolchains(&dir),
            vec!["arm-zephyr-eabi", "riscv64-zephyr-elf"]
        );

        // Older bundles keep them in the root, beside cmake/ and
        // hosttools/ --- which is why that case filters by name.
        let flat = dir.join("flat");
        std::fs::create_dir_all(flat.join("arm-zephyr-eabi")).unwrap();
        std::fs::create_dir_all(flat.join("cmake")).unwrap();
        std::fs::create_dir_all(flat.join("hosttools")).unwrap();
        assert_eq!(installed_toolchains(&flat), vec!["arm-zephyr-eabi"]);

        // A bundle that is not there answers nothing, never panics.
        assert!(installed_toolchains(&dir.join("nope")).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sdk_version_is_read_from_the_checkout() {
        let dir = std::env::temp_dir().join(format!("chiptui-sdkver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("zephyr")).unwrap();
        assert_eq!(sdk_version(&dir), None);
        std::fs::write(dir.join("zephyr/SDK_VERSION"), "1.0.1\n").unwrap();
        assert_eq!(sdk_version(&dir), Some("1.0.1".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_curated_toolchains_are_unique_and_carry_the_esp32_targets() {
        let mut sorted = TOOLCHAINS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), TOOLCHAINS.len(), "no duplicates");
        // ChipTUI's own focus: a list without these would be useless here.
        for needed in [
            "xtensa-espressif_esp32_zephyr-elf",
            "xtensa-espressif_esp32s3_zephyr-elf",
            "riscv64-zephyr-elf",
            "arm-zephyr-eabi",
        ] {
            assert!(TOOLCHAINS.contains(&needed), "{needed} must be offered");
        }
    }
}
