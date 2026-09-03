//! Build variants and out-of-tree board roots.
//!
//! A Zephyr application routinely has more than one target: the real board
//! and Zephyr's own `native_sim`, each with its own Kconfig fragment, its
//! own devicetree overlay and --- crucially --- its own build directory, so
//! building one does not discard the other. Upstream has no name for that
//! pairing; the convention every project reinvents is
//!
//! ```text
//! west build -b <board target> [-d <build dir>] [--shield <shield>]
//! ```
//!
//! with `prj.conf` holding only what every target can build and
//! `boards/<qualifier with '/' as '_'>.conf|.overlay` holding the rest ---
//! files Zephyr picks up by *name*, with no flag involved. A [`Variant`] is
//! that pairing given a name, so the dashboard can offer it as one answer
//! instead of three.
//!
//! Nothing here writes anything. A project may declare its variants in its
//! own `chiptui.toml` (read, never created --- `SPEC.md` §7), and a project
//! that declares none has them *discovered* from the two places the
//! convention already leaves them: the build directories it has built
//! before, and the fragments under `boards/`.

use std::path::{Path, PathBuf};

use super::yaml;

/// A module manifest's location relative to the module root.
const MODULE_MANIFEST: &str = "zephyr/module.yml";

/// Where a variant's definition came from --- shown on the picker's rows,
/// and the reason a declared list is never merged with a discovered one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantOrigin {
    /// A `[[variant]]` block in the project's own `chiptui.toml`.
    Declared,
    /// Inferred from the project's build directories and `boards/`
    /// fragments.
    Discovered,
}

impl VariantOrigin {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Declared => "chiptui.toml",
            Self::Discovered => "discovered",
        }
    }
}

/// One named build configuration of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    /// The short name the row and the picker show (`hardware`, `sim`).
    pub name: String,
    /// The full board target `west build -b` takes ---
    /// `native_sim/native/64`, not `native_sim`. `None` only for a declared
    /// variant that deliberately leaves the board open.
    pub board: Option<String>,
    /// The optional shield, `west build --shield`.
    pub shield: Option<String>,
    /// The build directory `west build -d` targets. Keeping one per
    /// variant is the whole point: a shared `build/` means every switch is
    /// a reconfigure.
    pub build_dir: String,
    pub origin: VariantOrigin,
}

impl Variant {
    /// Whether this variant runs on the host rather than on a board.
    ///
    /// Zephyr's host targets are `native_sim` (and its legacy `native_posix`
    /// sibling) plus `unit_testing`; the test names them by prefix because
    /// the qualifier varies (`native_sim/native`, `native_sim/native/64`).
    /// A host build has nothing to flash and an executable to run instead,
    /// which is the one place the dashboard's action list branches on a
    /// variant.
    pub fn is_simulator(&self) -> bool {
        self.board.as_deref().is_some_and(is_simulator_target)
    }

    /// The host executable a simulator variant builds, relative to the
    /// project root. `zephyr.exe` is the name Zephyr gives it on every
    /// platform --- it is an ELF, not a Windows binary.
    pub fn executable(&self, root: &Path) -> PathBuf {
        root.join(&self.build_dir).join("zephyr").join("zephyr.exe")
    }
}

/// Whether a board target runs on the host rather than on a board --- see
/// [`Variant::is_simulator`], which is this test applied to a variant's own
/// answer.
pub fn is_simulator_target(board: &str) -> bool {
    let head = board.split('/').next().unwrap_or(board);
    matches!(head, "native_sim" | "native_posix" | "unit_testing")
}

/// Everything the discovery needs to know about the boards `west` can
/// build: the catalogue the picker already fetched. A qualifier-expanded
/// target list is exactly what a `boards/<stem>.conf` has to be matched
/// against, since the stem *is* a target with `/` written as `_`.
pub type Catalogue<'a> = &'a [String];

/// The project's build variants, in the order they should be offered.
///
/// A `chiptui.toml` that declares any wins outright --- a hand-written list
/// is the most specific answer there is, and merging it with guesses would
/// make it impossible to *remove* a variant. Otherwise the two conventional
/// sources are merged (see [`discover`]).
pub fn variants(root: &Path, declared: &[Variant], catalogue: Catalogue<'_>) -> Vec<Variant> {
    if declared.is_empty() {
        discover(root, catalogue)
    } else {
        declared.to_vec()
    }
}

/// Infers the project's variants from the two places the convention leaves
/// them, strongest first:
///
/// 1. **the build directories it already has.** `<dir>/CMakeCache.txt`
///    names the exact board string and shield that configuration used, so a
///    project that has ever been built answers this question itself, with
///    no catalogue and no guessing.
/// 2. **`boards/<stem>.conf|.overlay`.** Zephyr picks these up by name:
///    the stem is the board target with `/` written as `_`. Recovering the
///    target from the stem needs the catalogue, because `_` is also a legal
///    character *inside* a board name (`native_sim_native_64` is
///    `native_sim/native/64`, not `native/sim/native/64`), so the stem is
///    matched against real targets rather than split on a rule.
///
/// A target found in both keeps the build directory it really has. One
/// found only under `boards/` gets a derived directory, which is where it
/// will land on its first build.
///
/// Returns an empty list when neither source says anything --- a project
/// with one board and one `build/` has no variants to choose between, and
/// inventing a list of one would add a question where there is none.
pub fn discover(root: &Path, catalogue: Catalogue<'_>) -> Vec<Variant> {
    let mut found: Vec<Variant> = Vec::new();

    for build_dir in build_dirs(root) {
        let Some(target) = crate::build::cached_target(root, &build_dir) else {
            continue;
        };
        if found.iter().any(|v| {
            v.board
                .as_deref()
                .is_some_and(|board| same_board(board, &target.board))
        }) {
            continue;
        }
        found.push(Variant {
            name: variant_name(&build_dir, &target.board),
            board: Some(target.board),
            shield: target.shield,
            build_dir,
            origin: VariantOrigin::Discovered,
        });
    }

    for target in fragment_targets(root, catalogue) {
        if found.iter().any(|v| {
            v.board
                .as_deref()
                .is_some_and(|board| same_board(board, &target))
        }) {
            continue;
        }
        let build_dir = free_build_dir(&found, &target);
        found.push(Variant {
            name: variant_name(&build_dir, &target),
            board: Some(target),
            shield: None,
            build_dir,
            origin: VariantOrigin::Discovered,
        });
    }

    if found.len() < 2 {
        return Vec::new();
    }
    dedupe_names(&mut found);
    found
}

/// Whether two board strings name the same board.
///
/// A build directory's cache records what `west build -b` was *given*,
/// which for a board with one cpucluster is usually the bare name
/// (`xiao_esp32c3`), while the catalogue always answers the qualified
/// target (`xiao_esp32c3/esp32c3`). Comparing the strings would list the
/// same board twice --- once from the directory it was built in, once from
/// its own `boards/` fragment.
///
/// Only a *bare* name (no `/` at all) may stand for a qualified one.
/// Comparing heads outright would merge `native_sim/native` with
/// `native_sim/native/64`, which are two real, different targets.
fn same_board(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let covers =
        |bare: &str, target: &str| !bare.contains('/') && target.split('/').next() == Some(bare);
    covers(a, b) || covers(b, a)
}

/// The project's build directories: immediate subdirectories whose name
/// starts with `build` and that hold a CMake cache. The name filter is what
/// keeps a `src/` or a `docs/` from being stat-ed for a cache, and `build`
/// is the prefix every convention in the wild uses (`build`, `build_sim`,
/// `build-sim`). `build` sorts first when present --- it is the default
/// `west build` targets, so it leads the list.
fn build_dirs(root: &Path) -> Vec<String> {
    let mut dirs: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("build"))
        .collect();
    dirs.sort();
    dirs.dedup();
    if let Some(index) = dirs
        .iter()
        .position(|name| name == crate::build::DEFAULT_BUILD_DIR)
    {
        dirs.swap(0, index);
    }
    dirs
}

/// The board targets the project's `boards/` fragments name, in file-name
/// order.
///
/// Zephyr resolves these files by *name*, and it accepts two spellings:
/// the board's bare name (`xiao_esp32c3.conf`) and the full target with
/// `/` written as `_` (`native_sim_native_64.conf`). Both appear in the
/// wild --- often in the same project --- so both are matched here, with
/// the qualified form preferred: it names exactly one target, while a bare
/// name covers every qualifier the board has and the first is the only
/// defensible pick.
///
/// A stem matching nothing in the catalogue is dropped rather than guessed
/// at: offering a board `west build -b` would reject is worse than
/// offering nothing. That is also why the catalogue is required --- `_` is
/// legal *inside* a board name, so no rule splits `native_sim_native_64`
/// into `native_sim/native/64` without knowing the real targets.
fn fragment_targets(root: &Path, catalogue: Catalogue<'_>) -> Vec<String> {
    let mut stems: Vec<String> = std::fs::read_dir(root.join("boards"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "conf" || ext == "overlay")
        })
        .filter_map(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .collect();
    stems.sort();
    stems.dedup();

    let mut targets = Vec::new();
    for stem in stems {
        let resolved = catalogue
            .iter()
            .find(|target| target.replace('/', "_") == stem)
            .or_else(|| {
                catalogue
                    .iter()
                    .find(|target| target.split('/').next().unwrap_or(target) == stem)
            });
        if let Some(target) = resolved
            && !targets.contains(target)
        {
            targets.push(target.clone());
        }
    }
    // Hardware first, so the conventional `build/` --- what a bare `west
    // build` targets --- lands on the board rather than on the simulator.
    // Stable, so file order still decides within each group.
    targets.sort_by_key(|target| is_simulator_target(target));
    targets
}

/// A build directory for a target that has none yet: `build` while it is
/// free, else `build-<short name>` --- the spelling the projects in the
/// wild already use, and one that cannot collide with a sibling variant.
fn free_build_dir(found: &[Variant], target: &str) -> String {
    let default = crate::build::DEFAULT_BUILD_DIR.to_string();
    if !found.iter().any(|v| v.build_dir == default) {
        return default;
    }
    let short = short_name(target);
    let mut candidate = format!("build-{short}");
    let mut suffix = 2;
    while found.iter().any(|v| v.build_dir == candidate) {
        candidate = format!("build-{short}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// The variant's display name: the build directory's own suffix when it has
/// one (`build_sim` and `build-sim` both read `sim`, which is what their
/// authors meant), and otherwise a short name off the board. The default
/// `build` gets the board's short name too --- "build" names the directory,
/// not the target.
fn variant_name(build_dir: &str, board: &str) -> String {
    let suffix = build_dir
        .strip_prefix("build")
        .map(|rest| rest.trim_start_matches(['-', '_']))
        .unwrap_or("");
    if suffix.is_empty() {
        short_name(board)
    } else {
        suffix.to_string()
    }
}

/// A board target's short name: `native_sim/native/64` reads `sim`,
/// `xiao_esp32c3/esp32c3` reads `xiao_esp32c3`. The qualifier is dropped
/// because it is the same word repeated, and `native_sim` is spelled `sim`
/// because that is what every project calls this variant.
fn short_name(board: &str) -> String {
    let head = board.split('/').next().unwrap_or(board);
    match head {
        "native_sim" | "native_posix" => "sim".to_string(),
        "unit_testing" => "test".to_string(),
        other => other.to_string(),
    }
}

/// Makes the names unique, since two variants may derive the same one (two
/// build directories for the same short board name). A duplicate falls back
/// to its build directory, which is unique by construction.
fn dedupe_names(variants: &mut [Variant]) {
    for index in 1..variants.len() {
        if variants[..index]
            .iter()
            .any(|earlier| earlier.name == variants[index].name)
        {
            variants[index].name = variants[index].build_dir.clone();
        }
    }
}

/// Extra board search roots this project contributes, nearest first.
///
/// A board Zephyr does not ship reaches a build through a *module*: a
/// directory carrying `zephyr/module.yml` whose `build.settings.board_root`
/// names where its `boards/` tree lives, pulled into the build by the
/// application's own `CMakeLists.txt`
/// (`list(APPEND ZEPHYR_EXTRA_MODULES ...)`). That is enough for
/// `west build`, and it is *not* enough for `west boards`, which seeds its
/// roots from `ZEPHYR_BASE` plus the manifest's modules and so never sees a
/// module the project reaches by CMake alone. These roots are what close
/// that gap --- for the listing only.
///
/// The walk starts at `root` and climbs, stopping when it leaves `stop_at`
/// (the configured projects folder) or runs out of parents: an application
/// in `repo/app/` finds the module at `repo/`, which is the layout that
/// makes the board committable as an upstream pull request later.
pub fn board_roots(root: &Path, stop_at: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut dir = Some(root);
    while let Some(current) = dir {
        if let Some(board_root) = module_board_root(current)
            && !roots.contains(&board_root)
        {
            roots.push(board_root);
        }
        if stop_at.is_some_and(|stop| current == stop) {
            break;
        }
        dir = current.parent();
    }
    roots
}

/// The board root `dir`'s module manifest declares, resolved against the
/// module directory. `None` when `dir` is not a module, or is one that
/// contributes no board root.
///
/// The key is `build.settings.board_root`, and the nesting is
/// load-bearing: west's `scripts/zephyr_module.py::process_settings` reads
/// the block under `build:` and silently ignores a top-level `settings:`,
/// so a manifest with the latter has no board root at all --- which is what
/// this function must report, rather than being generous about where it
/// looks.
fn module_board_root(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join(MODULE_MANIFEST)).ok()?;
    let entries = yaml::read_entries(&text);
    let declared = yaml::scalar(&entries, "build.settings.board_root")?;
    let resolved = normalize(&dir.join(declared));
    // The root is the directory *containing* `boards/`; a manifest pointing
    // somewhere without one contributes nothing and must not be passed to
    // west as a root.
    resolved.join("boards").is_dir().then_some(resolved)
}

/// Collapses the `.` and `..` components a manifest's relative root
/// introduces (`board_root: .` is the common spelling), without touching
/// the filesystem --- `canonicalize` would resolve symlinks too, and a
/// workspace assembled out of symlinked checkouts is normal.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-variants-{tag}-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The real layout: the repository is the module, the application is a
    /// subdirectory, and the board root is found by climbing.
    #[test]
    fn a_module_above_the_application_contributes_its_board_root() {
        let repo = fixture("module");
        std::fs::create_dir_all(repo.join("zephyr")).unwrap();
        std::fs::create_dir_all(repo.join("boards/lilygo/ttgo_t_display_s3")).unwrap();
        std::fs::create_dir_all(repo.join("app")).unwrap();
        std::fs::write(
            repo.join(MODULE_MANIFEST),
            "name: ttgo-t-display-s3\nbuild:\n  cmake: .\n  kconfig: Kconfig\n  \
             settings:\n    board_root: .\n    dts_root: .\n",
        )
        .unwrap();

        assert_eq!(
            board_roots(&repo.join("app"), Some(repo.parent().unwrap())),
            vec![normalize(&repo)],
            "the application's module is one directory up"
        );
    }

    /// A manifest that puts `settings:` at the top level parses without
    /// error and declares nothing --- west ignores it the same way, and
    /// reporting a root here would make the picker offer a board `west
    /// build` cannot find.
    #[test]
    fn a_top_level_settings_block_contributes_nothing() {
        let repo = fixture("toplevel");
        std::fs::create_dir_all(repo.join("zephyr")).unwrap();
        std::fs::create_dir_all(repo.join("boards/acme/thing")).unwrap();
        std::fs::write(
            repo.join(MODULE_MANIFEST),
            "name: acme\nsettings:\n  board_root: .\n",
        )
        .unwrap();
        assert!(board_roots(&repo, None).is_empty());
    }

    /// A module whose declared root holds no `boards/` is not a board root.
    #[test]
    fn a_module_without_a_boards_tree_is_not_a_root() {
        let repo = fixture("noboards");
        std::fs::create_dir_all(repo.join("zephyr")).unwrap();
        std::fs::write(
            repo.join(MODULE_MANIFEST),
            "name: acme\nbuild:\n  settings:\n    board_root: .\n",
        )
        .unwrap();
        assert!(board_roots(&repo, None).is_empty());
    }

    /// A plain application --- the common case --- contributes no roots, so
    /// the list commands stay exactly what they were.
    #[test]
    fn a_project_with_no_module_contributes_no_roots() {
        let dir = fixture("plain");
        std::fs::create_dir_all(dir.join("boards")).unwrap();
        assert!(board_roots(&dir, None).is_empty());
    }

    /// Writes a configured build directory: the two cache entries `west
    /// build` leaves behind, in the classic (non-sysbuild) location.
    fn built(root: &Path, dir: &str, board: &str, shield: Option<&str>) {
        std::fs::create_dir_all(root.join(dir).join("zephyr")).unwrap();
        let mut cache = format!("CMAKE_HOME_DIRECTORY:INTERNAL=/x\nCACHED_BOARD:STRING={board}\n");
        if let Some(shield) = shield {
            cache.push_str(&format!("SHIELD:STRING={shield}\n"));
        }
        std::fs::write(root.join(dir).join("zephyr/CMakeCache.txt"), cache).unwrap();
    }

    fn fragment(root: &Path, stem: &str) {
        std::fs::create_dir_all(root.join("boards")).unwrap();
        std::fs::write(root.join("boards").join(format!("{stem}.conf")), "").unwrap();
    }

    fn catalogue() -> Vec<String> {
        [
            "xiao_esp32c3/esp32c3",
            "native_sim/native",
            "native_sim/native/64",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// The `esp32c3-round-display` layout, recovered with no configuration
    /// at all: two build directories, and the hardware one carries a shield
    /// that a board-only read would have dropped.
    #[test]
    fn two_built_directories_become_two_variants_shield_included() {
        let root = fixture("built");
        built(
            &root,
            "build",
            "xiao_esp32c3",
            Some("seeed_xiao_round_display"),
        );
        built(&root, "build_sim", "native_sim/native/64", None);

        let found = discover(&root, &catalogue());
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "xiao_esp32c3");
        assert_eq!(found[0].build_dir, "build");
        assert_eq!(found[0].shield.as_deref(), Some("seeed_xiao_round_display"));
        assert!(!found[0].is_simulator());
        // `build_sim` and `build-sim` both mean "sim" to their authors.
        assert_eq!(found[1].name, "sim");
        assert_eq!(found[1].build_dir, "build_sim");
        assert_eq!(found[1].board.as_deref(), Some("native_sim/native/64"));
        assert!(found[1].is_simulator());
    }

    /// A fresh clone has no build directories; the `boards/` fragments are
    /// what name the targets, and the underscored stem is matched against
    /// the real catalogue rather than split on a rule --- `native_sim` is
    /// itself a name with an underscore in it.
    #[test]
    fn boards_fragments_name_the_targets_through_the_catalogue() {
        let root = fixture("fragments");
        fragment(&root, "xiao_esp32c3");
        fragment(&root, "native_sim_native_64");
        fragment(&root, "not_a_board_at_all");

        let found = discover(&root, &catalogue());
        let targets: Vec<&str> = found.iter().filter_map(|v| v.board.as_deref()).collect();
        assert_eq!(
            targets,
            vec!["xiao_esp32c3/esp32c3", "native_sim/native/64"],
            "hardware leads, whatever order the file names fall in"
        );
        // The board gets the default directory --- what a bare `west build`
        // targets --- and the simulator a derived one it will land in on
        // its first build.
        assert_eq!(found[0].build_dir, "build");
        assert_eq!(found[1].build_dir, "build-sim");
    }

    /// A target with a build directory keeps it; one with only a fragment
    /// gets a derived name that cannot collide with it.
    #[test]
    fn a_built_target_keeps_its_directory_and_the_other_gets_a_free_one() {
        let root = fixture("merge");
        built(&root, "build", "xiao_esp32c3/esp32c3", None);
        fragment(&root, "xiao_esp32c3_esp32c3");
        fragment(&root, "native_sim_native_64");

        let found = discover(&root, &catalogue());
        assert_eq!(found.len(), 2, "the built target is not listed twice");
        assert_eq!(found[0].build_dir, "build");
        assert_eq!(found[1].build_dir, "build-sim");
    }

    /// One target is not a choice. A project with a single board must not
    /// grow a picker offering it alone.
    /// The real `esp32c3-round-display` shape: the build cache records the
    /// *bare* board name `west build -b` was given, while the catalogue
    /// answers the qualified target its own `boards/` fragment resolves to.
    /// Comparing the strings listed the same board twice.
    #[test]
    fn a_bare_cached_board_and_its_qualified_target_are_one_variant() {
        let root = fixture("bare");
        built(
            &root,
            "build",
            "xiao_esp32c3",
            Some("seeed_xiao_round_display"),
        );
        built(&root, "build_sim", "native_sim/native/64", None);
        fragment(&root, "xiao_esp32c3");
        fragment(&root, "native_sim_native_64");

        let found = discover(&root, &catalogue());
        assert_eq!(found.len(), 2, "{found:#?}");
        assert_eq!(found[0].board.as_deref(), Some("xiao_esp32c3"));
        assert_eq!(found[1].board.as_deref(), Some("native_sim/native/64"));
    }

    /// Two qualifiers of the same board are two real targets, so the
    /// bare-name rule must not merge them.
    #[test]
    fn two_qualifiers_of_one_board_stay_two_targets() {
        assert!(same_board("xiao_esp32c3", "xiao_esp32c3/esp32c3"));
        assert!(same_board("xiao_esp32c3/esp32c3", "xiao_esp32c3"));
        assert!(!same_board("native_sim/native", "native_sim/native/64"));
        assert!(!same_board("xiao_esp32c3", "xiao_ble"));
    }

    #[test]
    fn a_single_target_is_no_variant_list() {
        let root = fixture("single");
        built(&root, "build", "xiao_esp32c3", None);
        assert!(discover(&root, &catalogue()).is_empty());
        assert!(discover(&root, &[]).is_empty());
    }

    /// A declared list wins outright: merging would make a variant
    /// impossible to *remove* from a project that names its own.
    #[test]
    fn a_declared_list_is_never_merged_with_discovery() {
        let root = fixture("declared");
        built(&root, "build", "xiao_esp32c3", None);
        built(&root, "build_sim", "native_sim/native/64", None);
        let declared = vec![Variant {
            name: "hardware".into(),
            board: Some("ttgo_t_display_s3/esp32s3/procpu".into()),
            shield: None,
            build_dir: "build".into(),
            origin: VariantOrigin::Declared,
        }];
        assert_eq!(variants(&root, &declared, &catalogue()), declared);
        // With none declared, discovery answers.
        assert_eq!(variants(&root, &[], &catalogue()).len(), 2);
    }

    #[test]
    fn a_simulator_target_is_recognised_by_its_head_not_its_qualifier() {
        let sim = |board: &str| Variant {
            name: "v".into(),
            board: Some(board.into()),
            shield: None,
            build_dir: "build".into(),
            origin: VariantOrigin::Discovered,
        };
        assert!(sim("native_sim/native/64").is_simulator());
        assert!(sim("native_sim").is_simulator());
        assert!(sim("unit_testing/unit_testing").is_simulator());
        assert!(!sim("xiao_esp32c3/esp32c3").is_simulator());
        // `native_sim` is the head, never a substring elsewhere.
        assert!(!sim("acme_native_sim/soc").is_simulator());
    }
}
