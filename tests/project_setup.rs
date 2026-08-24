//! The empty-project prompt end to end (`SPEC.md` §7).
//!
//! Mirrors `tests/flash_view.rs`: drives `App` through real key events
//! against a real (empty) temp directory, then checks that the answer was
//! recorded in the user config and that a fresh `ProjectManager` picks it up
//! without asking again.
//!
//! Every case redirects the home directory (`App::set_home_dir`) before
//! answering the prompt: answering *writes* to the user config, and a test
//! must never reach the developer's real `~/.config/chiptui/config.toml`.

use std::path::{Path, PathBuf};

use chiptui::app::{App, Overlay};
use chiptui::backend::BackendKind;
use chiptui::event::AppEvent;
use chiptui::project::{DetectionSource, ProjectManager, config};
use chiptui::settings::{self, ProjectRegistry};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A temp directory holding both the project and the fake home the config
/// is written into, so nothing escapes into the real one.
struct TempDir {
    path: PathBuf,
    home: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "chiptui-project-setup-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("project");
        let home = root.join("home");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Self { path, home }
    }

    fn app(&self) -> App {
        let mut app = App::new(&self.path);
        app.set_home_dir(&self.home);
        app
    }

    fn config_dir(&self) -> PathBuf {
        self.home.join(".config")
    }

    fn registry(&self) -> ProjectRegistry {
        ProjectRegistry::load(&self.config_dir(), &self.home)
    }

    fn root(&self) -> &Path {
        self.path.parent().unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.root());
    }
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn an_empty_directory_prompts_and_records_the_choice() {
    let dir = TempDir::new("empty");
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();

    assert_eq!(app.overlay, Some(Overlay::ProjectSetup { selected: 0 }));
    assert_eq!(app.manager.selected_kind(), None);

    // Move to Zephyr and accept.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, None);
    assert_eq!(app.manager.selected_kind(), Some(BackendKind::Zephyr));
    assert!(
        !dir.path.join(config::FILE_NAME).exists(),
        "the project directory gets no config file of ours"
    );

    let entry = dir.registry().entry_for(&dir.path).cloned();
    let entry = entry.expect("the project should be recorded in the user config");
    assert_eq!(entry.backend, BackendKind::Zephyr);
    assert_eq!(entry.name, "project");
    assert!(entry.last_opened.is_some(), "recorded as opened");

    // A fresh manager over the same directory needs no prompt.
    let mut manager = ProjectManager::new(&dir.path);
    manager.set_known_projects(dir.registry());
    let detection = manager.detect().unwrap();
    assert_eq!(detection.source, DetectionSource::Registered);
    assert_eq!(manager.selected_kind(), Some(BackendKind::Zephyr));
}

#[test]
fn choosing_micropython_creates_the_starting_layout() {
    let dir = TempDir::new("layout");
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();

    app.handle(key(KeyCode::Enter)); // MicroPython is the first option

    assert!(dir.path.join("src").is_dir());
    assert!(dir.path.join("firmware").is_dir());
    assert!(dir.path.join("src/main.py").is_file());
    assert!(dir.path.join("src/boot.py").is_file());
    let requirements = std::fs::read_to_string(dir.path.join("requirements.txt"))
        .expect("the Dependencies row's file starts in the project");
    assert!(
        requirements.starts_with("# MicroPython package requirements"),
        "the file documents its own grammar:\n{requirements}"
    );
}

#[test]
fn choosing_zephyr_creates_an_application_west_can_build() {
    let dir = TempDir::new("zephyr-layout");
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();

    app.handle(key(KeyCode::Down)); // Zephyr
    app.handle(key(KeyCode::Enter));

    let cmake = std::fs::read_to_string(dir.path.join("CMakeLists.txt")).unwrap();
    assert!(cmake.contains("find_package(Zephyr"), "{cmake}");
    assert!(cmake.contains("project(project)"), "named after the folder");
    assert!(dir.path.join("prj.conf").is_file());
    assert!(dir.path.join("src/main.c").is_file());
    assert!(
        !dir.path.join("firmware").exists(),
        "the MicroPython layout belongs to MicroPython"
    );
}

#[test]
fn an_existing_file_is_never_overwritten_by_the_scaffold() {
    let dir = TempDir::new("keep");
    std::fs::create_dir_all(dir.path.join("src")).unwrap();
    std::fs::write(dir.path.join("src/main.c"), "mine\n").unwrap();

    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();
    app.handle(key(KeyCode::Down)); // Zephyr
    app.handle(key(KeyCode::Enter));

    assert_eq!(
        std::fs::read_to_string(dir.path.join("src/main.c")).unwrap(),
        "mine\n"
    );
    assert!(
        dir.path.join("CMakeLists.txt").is_file(),
        "the rest is written"
    );
}

#[test]
fn choosing_micropython_also_scans_for_a_device() {
    // MicroPython gains `Capability::Filesystem` the moment it is selected,
    // so the scaffold prompt's answer should not leave the Dashboard sitting
    // on "not scanned" any more than the startup scan or the `o` picker do.
    let dir = TempDir::new("scan-on-choice");
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();
    assert_eq!(app.overlay, Some(Overlay::ProjectSetup { selected: 0 }));

    app.handle(key(KeyCode::Enter)); // MicroPython is the first option

    assert_eq!(app.manager.selected_kind(), Some(BackendKind::MicroPython));
    assert!(
        app.browser.is_some(),
        "a scan needs somewhere to land its result"
    );
}

#[test]
fn a_confidently_detected_project_is_never_prompted() {
    let dir = TempDir::new("confident");
    std::fs::write(dir.path.join("boot.py"), "").unwrap();
    std::fs::write(dir.path.join("main.py"), "").unwrap();

    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();

    assert_eq!(app.overlay, None, "MicroPython was detected confidently");
    assert_eq!(app.manager.selected_kind(), Some(BackendKind::MicroPython));
    assert!(!dir.path.join(config::FILE_NAME).exists());
}

#[test]
fn a_project_the_config_already_names_is_never_prompted() {
    let dir = TempDir::new("registered");
    settings::record_project(
        &settings::user_config_path(&dir.config_dir()),
        settings::ProjectEntry::new(&dir.path, BackendKind::Zephyr),
    )
    .unwrap();
    // Nothing in the directory itself says Zephyr.
    std::fs::write(dir.path.join("notes.txt"), "hi").unwrap();

    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();

    assert_eq!(app.overlay, None);
    assert_eq!(app.manager.selected_kind(), Some(BackendKind::Zephyr));
}

#[test]
fn dismissing_the_prompt_writes_nothing() {
    let dir = TempDir::new("dismiss");
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();
    assert!(app.overlay.is_some());

    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
    assert_eq!(app.manager.selected_kind(), None);
    assert!(!dir.path.join(config::FILE_NAME).exists());
    assert!(dir.registry().is_empty(), "nothing was recorded");
}

#[test]
fn re_detecting_reopens_the_prompt_until_resolved() {
    let dir = TempDir::new("re-detect");
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_setup();
    app.handle(key(KeyCode::Esc)); // dismiss without choosing

    app.handle(key(KeyCode::Char('r')));
    assert_eq!(
        app.overlay,
        Some(Overlay::ProjectSetup { selected: 0 }),
        "still unresolved, so re-detecting asks again"
    );
}

#[test]
fn a_project_carrying_its_own_config_file_still_wins() {
    // The hybrid rule: ChipTUI no longer *writes* `chiptui.toml`, but a
    // project that carries one (checked in, shared by a team) is still read,
    // and it outranks whatever the registry remembers about that directory.
    let dir = TempDir::new("project-file");
    config::write(&dir.path, BackendKind::MicroPython).unwrap();
    settings::record_project(
        &settings::user_config_path(&dir.config_dir()),
        settings::ProjectEntry::new(&dir.path, BackendKind::Zephyr),
    )
    .unwrap();

    let mut app = dir.app();
    app.bootstrap();

    assert_eq!(app.manager.selected_kind(), Some(BackendKind::MicroPython));
    assert_eq!(
        app.manager.detection().unwrap().source,
        DetectionSource::Config
    );
}
