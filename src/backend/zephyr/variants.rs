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
//! own `chiptui.toml` (also written by explicit simulator preparation), and a project
//! also discovers existing build directories, merged by path. Fragments
//! under `boards/` can suggest targets during simulator preparation, but
//! are not complete configurations in the lifecycle selector.

use std::path::{Path, PathBuf};

use super::yaml;

/// A module manifest's location relative to the module root.
const MODULE_MANIFEST: &str = "zephyr/module.yml";

/// Where a variant's definition came from, shown in project configuration.
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
/// Declarations supply names and configurations not yet built. Existing
/// directories join them by path, with cached board/shield describing what
/// that directory actually holds. Fragments alone do not create choices.
///
/// `app` is the application directory when the project root is not itself
/// the application (the module-repository layout): the `boards/` fragments
/// and build directories are read there, beside the application's sources.
/// `None` for every project whose root is the
/// application.
pub fn variants(
    root: &Path,
    app: Option<&Path>,
    declared: &[Variant],
    catalogue: Catalogue<'_>,
) -> Vec<Variant> {
    let _ = catalogue; // Fragments suggest boards, not complete build configurations.
    let mut found = Vec::new();
    for variant in declared {
        if !super::configuration::belongs_to(root, app, &variant.build_dir) {
            continue;
        }
        let mut variant = variant.clone();
        if let Some(target) = crate::build::cached_target(root, &variant.build_dir) {
            variant.board = Some(target.board);
            variant.shield = target.shield;
        }
        if !found.iter().any(|v: &Variant| {
            normalize(&root.join(&v.build_dir)) == normalize(&root.join(&variant.build_dir))
        }) {
            found.push(variant);
        }
    }
    for variant in discover_all(root, app, &[]) {
        if !found.iter().any(|v| {
            normalize(&root.join(&v.build_dir)) == normalize(&root.join(&variant.build_dir))
        }) {
            found.push(variant);
        }
    }
    dedupe_names(&mut found);
    found.sort_by(|a, b| a.build_dir.cmp(&b.build_dir));
    // Preserve the conventional single-board panel; a lone non-default
    // directory or simulator must still become the lifecycle's target.
    if declared.is_empty()
        && found.len() == 1
        && found[0].build_dir == build_path(root, app, "build")
        && !found[0].is_simulator()
    {
        Vec::new()
    } else {
        found
    }
}

/// Infers the project's variants from the two places the convention leaves
/// them, strongest first:
///
/// 1. **the build directories it already has.** `<dir>/CMakeCache.txt`
///    names the exact board string and shield that configuration used, so a
///    project that has ever been built answers this question itself, with
///    no catalogue and no guessing. They live beside the application.
/// 2. **`boards/<stem>.conf|.overlay`.** Zephyr picks these up by name:
///    the stem is the board target with `/` written as `_`. Recovering the
///    target from the stem needs the catalogue, because `_` is also a legal
///    character *inside* a board name (`native_sim_native_64` is
///    `native_sim/native/64`, not `native/sim/native/64`), so the stem is
///    matched against real targets rather than split on a rule. They live
///    with the *application* (`app`), whose own target fragments sit beside
///    its sources.
///
/// A target found in both keeps the build directory it really has. One
/// found only under `boards/` gets a derived directory, which is where it
/// will land on its first build.
///
/// Returns an empty list when neither source says anything --- a project
/// with one board and one `build/` has no variants to choose between, and
/// inventing a list of one would add a question where there is none.
pub fn discover(root: &Path, app: Option<&Path>, catalogue: Catalogue<'_>) -> Vec<Variant> {
    let found = discover_all(root, app, catalogue);
    if found.len() < 2 { Vec::new() } else { found }
}

/// Preparation needs to retain a lone existing target too: introducing a
/// declaration must not discard the target hidden by the single-target UI.
pub fn discover_all(root: &Path, app: Option<&Path>, catalogue: Catalogue<'_>) -> Vec<Variant> {
    let mut found: Vec<Variant> = Vec::new();

    for build_dir in build_dirs(root, app) {
        let Some(target) = crate::build::cached_target(root, &build_dir) else {
            continue;
        };
        if !super::configuration::belongs_to(root, app, &build_dir) {
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

    for target in fragment_targets(app.unwrap_or(root), catalogue) {
        if found.iter().any(|v| {
            v.board
                .as_deref()
                .is_some_and(|board| same_board(board, &target))
        }) {
            continue;
        }
        let build_dir = free_build_dir(root, app, &found, &target);
        found.push(Variant {
            name: variant_name(&build_dir, &target),
            board: Some(target),
            shield: None,
            build_dir,
            origin: VariantOrigin::Discovered,
        });
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

/// A conventional build path, expressed relative to the repository when possible.
pub fn build_path(root: &Path, app: Option<&Path>, name: &str) -> String {
    let base = app.unwrap_or(root);
    base.strip_prefix(root)
        .unwrap_or(base)
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Immediate `build*` children of the application, expressed relative to the
/// project root. Cache validation happens in discovery. The default sorts first.
fn build_dirs(root: &Path, app: Option<&Path>) -> Vec<String> {
    let mut dirs: Vec<String> = std::fs::read_dir(app.unwrap_or(root))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("build"))
        .map(|name| build_path(root, app, &name))
        .collect();
    dirs.sort();
    dirs.dedup();
    if let Some(index) = dirs
        .iter()
        .position(|name| name == &build_path(root, app, crate::build::DEFAULT_BUILD_DIR))
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
fn free_build_dir(root: &Path, app: Option<&Path>, found: &[Variant], target: &str) -> String {
    let default = build_path(root, app, crate::build::DEFAULT_BUILD_DIR);
    if !found.iter().any(|v| v.build_dir == default) {
        return default;
    }
    let short = short_name(target);
    let mut candidate = build_path(root, app, &format!("build-{short}"));
    let mut suffix = 2;
    while found.iter().any(|v| v.build_dir == candidate) {
        candidate = build_path(root, app, &format!("build-{short}-{suffix}"));
        suffix += 1;
    }
    candidate
}

/// The project's existing Kconfig fragment for `board`, in either spelling
/// Zephyr accepts: the qualified stem first (`xiao_esp32c3_esp32c3.conf`
/// names exactly one target), then the bare name (`xiao_esp32c3.conf`,
/// which covers every qualifier the board has --- [`fragment_targets`]'
/// matching rule, read from the writing side). `None` when the project has
/// no fragment for the board at all.
///
/// The answer is relative to `root`, like [`Variant::build_dir`]: callers
/// that write hand it to `scaffold`'s project-rooted guard, and callers
/// that read join it themselves.
pub fn fragment_path(root: &Path, board: &str) -> Option<PathBuf> {
    let qualified = fragment_path_for(board);
    if root.join(&qualified).exists() {
        return Some(qualified);
    }
    let bare = board.split('/').next().unwrap_or(board);
    let bare_path = PathBuf::from("boards").join(format!("{bare}.conf"));
    (root.join(&bare_path).exists() && bare_path != qualified).then_some(bare_path)
}

/// Where a *new* fragment for `board` goes (relative to the project root):
/// the qualified spelling, which names exactly one target --- a fragment
/// written for a bare name would silently apply to every qualifier the
/// board has. Nothing is created here; this is the path a writer uses, not
/// the write.
pub fn fragment_path_for(board: &str) -> PathBuf {
    PathBuf::from("boards").join(format!("{}.conf", board.replace('/', "_")))
}

/// The variant's display name: the build directory's own suffix when it has
/// one (`build_sim` and `build-sim` both read `sim`, which is what their
/// authors meant), and otherwise a short name off the board. The default
/// `build` gets the board's short name too --- "build" names the directory,
/// not the target.
fn variant_name(build_dir: &str, board: &str) -> String {
    let build_dir = Path::new(build_dir)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(build_dir);
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

/// The extra roots the *list* commands search: everything
/// [`board_roots`] answers plus, when the project itself defines
/// boards, the project's own root.
///
/// The other way a board reaches a build has no manifest at all: a
/// `boards/` tree inside the application, pulled in by its
/// `CMakeLists.txt` with `list(APPEND BOARD_ROOT
/// ${CMAKE_CURRENT_SOURCE_DIR})`. A plain `west build` finds the board
/// that way, but `west boards` still does not, so the picker would
/// offer everything *except* the board the project exists to support.
/// The project root joins the listing for that case.
///
/// It joins the **listing only**. The configure's `-DBOARD_ROOT`
/// ([`board_roots`]) stays what the project itself declared: a root
/// this function invents could replace CMake's default there and hide
/// every stock board an application that never touched `BOARD_ROOT`
/// still builds for --- while the application's own declaration, when
/// it exists, already covers the build.
///
/// A root [`board_roots`] already answered (the application is its own
/// module, or a parent's manifest points here) is not added twice.
pub fn board_list_roots(root: &Path, stop_at: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = board_roots(root, stop_at);
    if !roots.iter().any(|known| known == root) && local_boards(root) {
        roots.insert(0, root.to_path_buf());
    }
    roots
}

/// Whether the project's own `boards/` directory holds a real board.
///
/// The directory carries the per-target fragments too
/// ([`fragment_path`]'s home), which are not boards, so the test looks
/// for the one file every board definition of the current format
/// carries: a `board.yml` inside a subdirectory, one or two levels
/// down (`boards/<board>/` and `boards/<vendor>/<board>/`).
fn local_boards(root: &Path) -> bool {
    let Ok(vendors) = std::fs::read_dir(root.join("boards")) else {
        return false;
    };
    vendors.flatten().any(|vendor| {
        std::fs::read_dir(vendor.path()).is_ok_and(|boards| {
            boards
                .flatten()
                .any(|board| board.path().join("board.yml").is_file())
        })
    })
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

    /// The soil-meter shape: the application defines its board in its own
    /// `boards/` tree --- `list(APPEND BOARD_ROOT ...)` in the
    /// `CMakeLists.txt`, no module manifest anywhere --- next to the
    /// per-target fragments the same directory carries. The listing must
    /// offer the board; the configure roots must not move, because the
    /// application's own CMake owns `BOARD_ROOT`.
    #[test]
    fn a_project_local_boards_tree_joins_the_listing() {
        let dir = fixture("local");
        std::fs::create_dir_all(dir.join("boards/lilygo/t_qt_pro")).unwrap();
        std::fs::write(dir.join("boards/lilygo/t_qt_pro/board.yml"), "").unwrap();
        std::fs::write(dir.join("boards/native_sim_native_64.conf"), "").unwrap();

        assert_eq!(
            board_list_roots(&dir, None),
            vec![dir.clone()],
            "the project root is the board root"
        );
        assert!(
            board_roots(&dir, None).is_empty(),
            "the listing is the only consumer of the local root"
        );
    }

    /// Fragments are not boards: a `boards/` directory holding only the
    /// per-target `.conf`/`.overlay` files --- or subdirectories without a
    /// `board.yml` --- contributes no root, or every fragment-style
    /// project would grow a listing root that adds nothing.
    #[test]
    fn a_boards_tree_without_a_board_definition_is_not_a_root() {
        let dir = fixture("fragments-only");
        std::fs::create_dir_all(dir.join("boards/lilygo/t_qt_pro")).unwrap();
        std::fs::write(dir.join("boards/native_sim_native_64.conf"), "").unwrap();
        std::fs::write(dir.join("boards/lilygo/t_qt_pro/Kconfig"), "").unwrap();
        assert!(board_list_roots(&dir, None).is_empty());
    }

    /// The application that is its own module already contributed its root
    /// through the manifest; the listing must not carry it twice.
    #[test]
    fn an_app_that_is_its_own_module_is_not_added_twice() {
        let dir = fixture("self-module");
        std::fs::create_dir_all(dir.join("zephyr")).unwrap();
        std::fs::create_dir_all(dir.join("boards/lilygo/t_qt_pro")).unwrap();
        std::fs::write(dir.join("boards/lilygo/t_qt_pro/board.yml"), "").unwrap();
        std::fs::write(
            dir.join(MODULE_MANIFEST),
            "name: acme\nbuild:\n  settings:\n    board_root: .\n",
        )
        .unwrap();

        assert_eq!(board_list_roots(&dir, None), vec![dir.clone()]);
        assert_eq!(board_roots(&dir, None), vec![dir.clone()]);
    }

    /// An ancestor holding a bare `boards/` tree --- no module manifest ---
    /// stays out: the local treatment is for the project's own directory,
    /// and a parent's tree is somebody else's.
    #[test]
    fn a_parent_with_a_bare_boards_tree_is_not_a_root() {
        let repo = fixture("parent");
        std::fs::create_dir_all(repo.join("boards/lilygo/t_qt_pro")).unwrap();
        std::fs::write(repo.join("boards/lilygo/t_qt_pro/board.yml"), "").unwrap();
        std::fs::create_dir_all(repo.join("app")).unwrap();
        assert!(board_list_roots(&repo.join("app"), None).is_empty());
    }

    /// Writes a configured build directory: the two cache entries `west
    /// build` leaves behind, in the classic (non-sysbuild) location.
    fn built(root: &Path, dir: &str, board: &str, shield: Option<&str>) {
        std::fs::create_dir_all(root.join(dir).join("zephyr")).unwrap();
        let mut cache = format!(
            "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCACHED_BOARD:STRING={board}\n",
            root.display()
        );
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

        let found = discover(&root, None, &catalogue());
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

    #[test]
    fn nested_application_owns_discovery_and_fragment_build_paths() {
        let root = fixture("nested-build-base");
        let app = root.join("app");
        built(&root, "build_old", "xiao_esp32c3", None);
        built(&app, "build", "xiao_esp32c3", None);
        built(&app, "build_sim", "native_sim/native/64", None);
        let found = variants(&root, Some(&app), &[], &[]);
        assert_eq!(
            found
                .iter()
                .map(|v| v.build_dir.as_str())
                .collect::<Vec<_>>(),
            ["app/build", "app/build_sim"]
        );
        assert_eq!(found[1].name, "sim");

        std::fs::remove_dir_all(app.join("build_sim")).unwrap();
        assert!(variants(&root, Some(&app), &[], &[]).is_empty());
        fragment(&app, "native_sim_native_64");
        let found = discover_all(&root, Some(&app), &catalogue());
        assert!(found.iter().all(|v| v.build_dir.starts_with("app/build")));
        assert_eq!(found.len(), 2);
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

        let found = discover(&root, None, &catalogue());
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

        let found = discover(&root, None, &catalogue());
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

        let found = discover(&root, None, &catalogue());
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
        assert!(discover(&root, None, &catalogue()).is_empty());
        assert!(discover(&root, None, &[]).is_empty());
    }

    /// A declaration names an existing directory without hiding its siblings.
    #[test]
    fn declarations_merge_by_directory_and_existing_caches_name_the_target() {
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
        let found = variants(&root, None, &declared, &catalogue());
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "hardware");
        assert_eq!(found[0].board.as_deref(), Some("xiao_esp32c3"));
        assert_eq!(found[1].build_dir, "build_sim");
        // With none declared, discovery answers.
        assert_eq!(variants(&root, None, &[], &catalogue()).len(), 2);
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

    #[test]
    fn a_new_fragment_goes_to_the_qualified_spelling() {
        assert_eq!(
            fragment_path_for("xiao_esp32c3/esp32c3"),
            PathBuf::from("boards/xiao_esp32c3_esp32c3.conf")
        );
        assert_eq!(
            fragment_path_for("xiao_esp32c3"),
            PathBuf::from("boards/xiao_esp32c3.conf"),
            "a bare name is already exactly one board"
        );
    }

    #[test]
    fn an_existing_fragment_is_found_in_either_spelling() {
        let root = fixture("fragment-find");
        let board = "xiao_esp32c3/esp32c3";
        assert_eq!(fragment_path(&root, board), None, "no fragment yet");

        std::fs::create_dir_all(root.join("boards")).unwrap();
        std::fs::write(root.join("boards/xiao_esp32c3.conf"), "# bare\n").unwrap();
        assert_eq!(
            fragment_path(&root, board),
            Some(PathBuf::from("boards/xiao_esp32c3.conf")),
            "the bare spelling covers every qualifier"
        );

        std::fs::write(
            root.join("boards/xiao_esp32c3_esp32c3.conf"),
            "# one target\n",
        )
        .unwrap();
        assert_eq!(
            fragment_path(&root, board),
            Some(PathBuf::from("boards/xiao_esp32c3_esp32c3.conf")),
            "the qualified spelling wins when both exist"
        );
    }
}
