//! The empty-project scaffold prompt end to end (`SPEC.md` §7).
//!
//! Mirrors `tests/flash_view.rs`: drives `App` through real key events
//! against a real (empty) temp directory, then checks the persisted
//! `chiptui.toml` is picked up automatically by a fresh `ProjectManager`.

use std::path::PathBuf;

use chiptui::app::{App, Overlay};
use chiptui::backend::BackendKind;
use chiptui::event::AppEvent;
use chiptui::project::{DetectionSource, ProjectManager, config};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "chiptui-project-setup-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn an_empty_directory_prompts_and_persists_the_choice() {
    let dir = TempDir::new("empty");
    let mut app = App::new(&dir.path);
    app.bootstrap();
    app.maybe_open_project_setup();

    assert_eq!(app.overlay, Some(Overlay::ProjectSetup { selected: 0 }));
    assert_eq!(app.manager.selected_kind(), None);

    // Move to Zephyr and accept.
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Enter));

    assert_eq!(app.overlay, None);
    assert_eq!(app.manager.selected_kind(), Some(BackendKind::Zephyr));

    let text = std::fs::read_to_string(dir.path.join(config::FILE_NAME)).unwrap();
    assert_eq!(config::parse(&text), Some(BackendKind::Zephyr));

    // A fresh manager over the same directory needs no prompt.
    let mut manager = ProjectManager::new(&dir.path);
    let detection = manager.detect().unwrap();
    assert_eq!(detection.source, DetectionSource::Config);
    assert_eq!(manager.selected_kind(), Some(BackendKind::Zephyr));
}

#[test]
fn choosing_micropython_creates_the_src_and_firmware_layout() {
    let dir = TempDir::new("layout");
    let mut app = App::new(&dir.path);
    app.bootstrap();
    app.maybe_open_project_setup();

    app.handle(key(KeyCode::Enter)); // MicroPython is the first option

    assert!(dir.path.join("src").is_dir());
    assert!(dir.path.join("firmware").is_dir());
}

#[test]
fn choosing_zephyr_creates_no_micropython_layout() {
    let dir = TempDir::new("no-layout");
    let mut app = App::new(&dir.path);
    app.bootstrap();
    app.maybe_open_project_setup();

    app.handle(key(KeyCode::Down)); // Zephyr
    app.handle(key(KeyCode::Enter));

    assert!(!dir.path.join("src").exists());
    assert!(!dir.path.join("firmware").exists());
}

#[test]
fn choosing_micropython_also_scans_for_a_device() {
    // MicroPython gains `Capability::Filesystem` the moment it is selected,
    // so the scaffold prompt's answer should not leave the Dashboard sitting
    // on "not scanned" any more than the startup scan or the `o` picker do.
    let dir = TempDir::new("scan-on-choice");
    let mut app = App::new(&dir.path);
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

    let mut app = App::new(&dir.path);
    app.bootstrap();
    app.maybe_open_project_setup();

    assert_eq!(app.overlay, None, "MicroPython was detected confidently");
    assert_eq!(app.manager.selected_kind(), Some(BackendKind::MicroPython));
    assert!(!dir.path.join(config::FILE_NAME).exists());
}

#[test]
fn dismissing_the_prompt_writes_nothing() {
    let dir = TempDir::new("dismiss");
    let mut app = App::new(&dir.path);
    app.bootstrap();
    app.maybe_open_project_setup();
    assert!(app.overlay.is_some());

    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
    assert_eq!(app.manager.selected_kind(), None);
    assert!(!dir.path.join(config::FILE_NAME).exists());
}

#[test]
fn re_detecting_reopens_the_prompt_until_resolved() {
    let dir = TempDir::new("re-detect");
    let mut app = App::new(&dir.path);
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
