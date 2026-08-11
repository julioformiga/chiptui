//! End-to-end project detection against real directories.
//!
//! The unit tests in `src/project/detect.rs` cover scoring against in-memory
//! snapshots; these cover the parts only the filesystem can exercise: reading
//! entries, reading file contents, and walking up to the project root.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use chiptui::backend::{BackendKind, BackendRegistry};
use chiptui::project::{DetectionOutcome, DetectionSource, ProjectManager, detect_from};

/// A temporary directory removed on drop, so tests leave no residue.
struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "chiptui-test-{tag}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp tree");
        Self { root }
    }

    fn file(&self, relative: &str, contents: &str) -> &Self {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
        self
    }

    fn dir(&self, relative: &str) -> &Self {
        fs::create_dir_all(self.root.join(relative)).expect("create dir");
        self
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A Zephyr application laid out the way `west` generates one.
fn zephyr_app(tag: &str) -> TempTree {
    let tree = TempTree::new(tag);
    tree.file(
        "CMakeLists.txt",
        "cmake_minimum_required(VERSION 3.20.0)\n\
         find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})\n\
         project(blinky)\n\
         target_sources(app PRIVATE src/main.c)\n",
    )
    .file("prj.conf", "CONFIG_GPIO=y\n")
    .file("src/main.c", "int main(void) { return 0; }\n")
    .dir("boards");
    tree
}

#[test]
fn detects_a_zephyr_application_on_disk() {
    let tree = zephyr_app("zephyr");
    let registry = BackendRegistry::with_builtin_backends();

    let detection = detect_from(&registry, tree.root()).expect("detection succeeds");

    assert_eq!(
        detection.outcome,
        DetectionOutcome::Detected(BackendKind::Zephyr)
    );
    assert_eq!(detection.root, tree.root());
    assert_eq!(detection.source, DetectionSource::Automatic);
    assert!(detection.confidence().unwrap() >= 0.9);
    // File *contents* were read, not just names.
    assert!(
        detection
            .scores
            .iter()
            .find(|score| score.kind == BackendKind::Zephyr)
            .unwrap()
            .signals
            .iter()
            .any(|signal| signal.id == "cmake-zephyr")
    );
}

#[test]
fn finds_the_project_root_from_a_nested_directory() {
    // SPEC.md §7: search upward from the current directory.
    let tree = zephyr_app("nested");
    let registry = BackendRegistry::with_builtin_backends();

    let detection = detect_from(&registry, &tree.path("src")).expect("detection succeeds");

    assert_eq!(
        detection.outcome,
        DetectionOutcome::Detected(BackendKind::Zephyr)
    );
    assert_eq!(detection.root, tree.root());
    assert!(
        detection.searched.len() >= 2,
        "the starting directory and its parent should both be recorded"
    );
    assert_eq!(detection.searched[0], tree.path("src"));
}

#[test]
fn detects_a_micropython_project_on_disk() {
    let tree = TempTree::new("micropython");
    tree.file("boot.py", "import network\n")
        .file("main.py", "print('hello')\n")
        .dir("lib");
    let registry = BackendRegistry::with_builtin_backends();

    let detection = detect_from(&registry, tree.root()).expect("detection succeeds");

    assert_eq!(
        detection.outcome,
        DetectionOutcome::Detected(BackendKind::MicroPython)
    );
}

#[test]
fn an_unrelated_directory_yields_no_backend() {
    let tree = TempTree::new("unrelated");
    tree.file("notes.md", "# notes\n").file("data.csv", "a,b\n");
    let registry = BackendRegistry::with_builtin_backends();

    let detection = detect_from(&registry, tree.root()).expect("detection succeeds");

    assert_eq!(detection.backend(), None);
    // Reaching the filesystem root is not an error, just an absence of evidence.
    assert!(!detection.searched.is_empty());
}

#[test]
fn a_missing_start_directory_is_reported_as_an_error() {
    let registry = BackendRegistry::with_builtin_backends();
    let missing = std::env::temp_dir().join("chiptui-does-not-exist-4f2a9c");

    let error = detect_from(&registry, &missing).expect_err("must fail");

    let message = error.to_string();
    assert!(
        message.contains("project detection"),
        "unhelpful message: {message}"
    );
    assert!(
        message.contains("chiptui-does-not-exist"),
        "message omits the path: {message}"
    );
}

#[test]
fn the_project_manager_exposes_capabilities_after_detection() {
    use chiptui::backend::Capability;

    let tree = zephyr_app("manager");
    let mut manager = ProjectManager::new(tree.path("src"));

    manager.detect().expect("detection succeeds");

    assert_eq!(manager.selected_kind(), Some(BackendKind::Zephyr));
    assert_eq!(manager.root(), Some(tree.root()));
    assert!(manager.capabilities().contains(Capability::Build));
    assert!(!manager.capabilities().contains(Capability::Repl));

    // An override survives re-detection.
    manager.set_override(Some(BackendKind::MicroPython));
    manager.detect().expect("detection succeeds");
    assert_eq!(manager.selected_kind(), Some(BackendKind::MicroPython));
    assert!(manager.capabilities().contains(Capability::Repl));
}
