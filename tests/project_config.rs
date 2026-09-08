//! The project configuration screen end to end (`SPEC.md` §7, §13).
//!
//! Drives `App` through real key events against real temp directories, then
//! checks what landed in the project's `chiptui.toml`, in the user config
//! and in the registry --- and, as often, what did *not*, since the window's
//! governing rule is that nothing reaches disk before it is applied.
//!
//! Every case redirects the home directory (`App::set_home_dir`) before
//! answering anything: applying writes to the user config, and a test must
//! never reach the developer's real `~/.config/chiptui/config.toml`.

use std::path::{Path, PathBuf};

use chiptui::app::{App, AppEvent, Overlay};
use chiptui::backend::{BackendKind, BackendRegistry};
use chiptui::project::{DetectionSource, ProjectManager, config};
use chiptui::project_config::{Cursor, ProjectConfigRow};
use chiptui::settings::{self, ProjectRegistry};
use chiptui::startup::{Route, route};
use ratatui::crossterm::event::KeyCode;

mod common;
use common::key;

/// A temp directory holding both the project and the fake home the configs
/// are written into, so nothing escapes into the real one.
struct TempDir {
    path: PathBuf,
    home: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "chiptui-project-config-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("project");
        let home = root.join("home");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Self { path, home }
    }

    /// The shape this whole feature exists for: a Zephyr repository whose
    /// root is an out-of-tree board *module*, with the application one
    /// directory down. Nothing at the top calls `find_package(Zephyr)`, so
    /// the root scores 0.25 against a 0.35 floor.
    fn into_board_module(self) -> Self {
        std::fs::create_dir_all(self.path.join("boards/lilygo")).unwrap();
        std::fs::create_dir_all(self.path.join("zephyr")).unwrap();
        std::fs::create_dir_all(self.path.join("app/src")).unwrap();
        std::fs::write(
            self.path.join("CMakeLists.txt"),
            "# the module contributes a board only\n",
        )
        .unwrap();
        std::fs::write(self.path.join("Kconfig"), "").unwrap();
        std::fs::write(
            self.path.join("zephyr/module.yml"),
            "name: board\nbuild:\n  settings:\n    board_root: .\n",
        )
        .unwrap();
        std::fs::write(
            self.path.join("app/CMakeLists.txt"),
            "find_package(Zephyr REQUIRED)\nproject(app)\n",
        )
        .unwrap();
        self
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

    fn file(&self) -> PathBuf {
        self.path.join(config::FILE_NAME)
    }

    fn text(&self) -> String {
        std::fs::read_to_string(self.file()).unwrap_or_default()
    }

    fn user_text(&self) -> String {
        std::fs::read_to_string(settings::user_config_path(&self.config_dir())).unwrap_or_default()
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

fn open(app: &mut App) {
    app.open_project_config(false);
    assert_eq!(app.overlay, Some(Overlay::ProjectConfig));
}

/// Walks the cursor to `row` from wherever it is.
fn go_to(app: &mut App, row: ProjectConfigRow) {
    for _ in 0..40 {
        let panel = app.project_config.as_ref().expect("the window is open");
        if panel.selected() == Some(row) {
            return;
        }
        app.handle(key(KeyCode::Down));
    }
    panic!("{row:?} is not a row of this window");
}

fn type_into(app: &mut App, row: ProjectConfigRow, value: &str) {
    go_to(app, row);
    app.handle(key(KeyCode::Enter));
    app.handle(key(KeyCode::Delete)); // replace rather than append
    for ch in value.chars() {
        app.handle(key(KeyCode::Char(ch)));
    }
    app.handle(key(KeyCode::Enter));
}

/// Picks Zephyr on the card strip. `←` from an unanswered strip lands on the
/// last card, which is the cheapest way to reach it.
fn pick_zephyr(app: &mut App) {
    app.handle(key(KeyCode::Home));
    app.handle(key(KeyCode::Left));
    assert_eq!(
        app.project_config.as_ref().unwrap().chosen(),
        Some(BackendKind::Zephyr)
    );
}

fn apply(app: &mut App) {
    app.handle(common::ctrl('s'));
    assert!(
        matches!(app.overlay, Some(Overlay::ConfirmApplyConfig { .. })),
        "applying reviews first: {:?}",
        app.overlay
    );
    app.handle(key(KeyCode::Char('y')));
}

#[test]
fn a_zephyr_module_root_opens_the_window_on_the_backend_cards() {
    let dir = TempDir::new("module").into_board_module();

    // The routing half: this directory used to be answered with a list of
    // other projects.
    assert_eq!(
        route(
            &dir.path,
            &BackendRegistry::with_builtin_backends(),
            &ProjectRegistry::default()
        ),
        Route::Open(dir.path.clone()),
    );

    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();
    assert_eq!(app.overlay, Some(Overlay::ProjectConfig));
    let panel = app.project_config.as_ref().unwrap();
    assert_eq!(
        panel.cursor(),
        Cursor::Cards,
        "the window opens on the question it exists to ask"
    );
    assert_eq!(panel.chosen(), None);
    assert!(!panel.file_exists(), "nothing written by opening it");
}

#[test]
fn choosing_a_backend_reveals_its_sections_and_writes_nothing() {
    let dir = TempDir::new("choose").into_board_module();
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();

    let general = app.project_config.as_ref().unwrap().rows().len();
    pick_zephyr(&mut app);

    let panel = app.project_config.as_ref().unwrap();
    assert!(
        panel.rows().len() > general,
        "the chosen backend's sections appear at once"
    );
    assert!(
        panel.rows().contains(&ProjectConfigRow::Heading(
            chiptui::project_config::Section::Zephyr
        )),
        "and they are that backend's: {:?}",
        panel.rows()
    );
    assert_eq!(
        panel.change_count(),
        1,
        "the choice is the one pending change"
    );
    assert!(!dir.file().exists(), "and it has not been written");
    assert_eq!(
        app.manager.selected_kind(),
        None,
        "nor applied to the session"
    );
}

#[test]
fn applying_writes_the_file_records_the_project_and_settles() {
    let dir = TempDir::new("apply").into_board_module();
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();
    pick_zephyr(&mut app);
    type_into(
        &mut app,
        ProjectConfigRow::ZephyrWorkspace,
        "/opt/zephyrproject",
    );
    assert_eq!(app.project_config.as_ref().unwrap().change_count(), 2);

    apply(&mut app);

    assert_eq!(app.manager.selected_kind(), Some(BackendKind::Zephyr));
    assert_eq!(
        dir.text(),
        "project_type = \"zephyr\"\n\n[zephyr]\nworkspace = \"/opt/zephyrproject\"\n"
    );
    let panel = app.project_config.as_ref().unwrap();
    assert_eq!(panel.change_count(), 0, "the transaction settled");
    assert_eq!(
        app.overlay,
        Some(Overlay::ProjectConfig),
        "and the window is handed back --- the overlay slot is one deep"
    );

    let entry = dir
        .registry()
        .entry_for(&dir.path)
        .cloned()
        .expect("recorded in the user config");
    assert_eq!(entry.backend, BackendKind::Zephyr);

    // A fresh manager reads the *file* first.
    let mut manager = ProjectManager::new(&dir.path);
    manager.set_known_projects(dir.registry());
    assert_eq!(manager.detect().unwrap().source, DetectionSource::Config);
    assert_eq!(manager.selected_kind(), Some(BackendKind::Zephyr));
}

#[test]
fn a_directory_that_already_holds_a_project_is_never_scaffolded() {
    let dir = TempDir::new("no-scaffold").into_board_module();
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();
    pick_zephyr(&mut app);
    assert!(
        app.config_scaffold().is_empty(),
        "and the review says so before the user answers it"
    );
    apply(&mut app);

    assert!(
        !dir.path.join("prj.conf").exists(),
        "the scaffold must not drop files into a repository that has one"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path.join("CMakeLists.txt")).unwrap(),
        "# the module contributes a board only\n",
        "and it overwrites nothing"
    );
}

#[test]
fn an_empty_directory_is_still_scaffolded_after_the_keys_are_written() {
    // The `mkdir x && cd x && chiptui` path. The transaction writes
    // `chiptui.toml` first, which is not a hidden entry --- so asking
    // whether the directory is empty *after* that write answers about a
    // directory this very apply had just filled, and the starting layout
    // the review promised never appeared.
    let dir = TempDir::new("scaffold");
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();
    pick_zephyr(&mut app);
    type_into(&mut app, ProjectConfigRow::ZephyrWorkspace, "/opt/ws");
    assert_eq!(
        app.config_scaffold(),
        vec![
            "CMakeLists.txt".to_string(),
            "prj.conf".to_string(),
            "src/main.c".to_string()
        ],
        "the review names the files it will create"
    );

    apply(&mut app);

    let cmake = std::fs::read_to_string(dir.path.join("CMakeLists.txt")).unwrap();
    assert!(cmake.contains("find_package(Zephyr"), "{cmake}");
    assert!(dir.path.join("prj.conf").is_file());
    assert!(dir.path.join("src/main.c").is_file());
}

#[test]
fn a_micropython_answer_scans_for_a_device() {
    let dir = TempDir::new("scan");
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();
    app.handle(key(KeyCode::Right)); // MicroPython, the first card
    apply(&mut app);

    assert_eq!(app.manager.selected_kind(), Some(BackendKind::MicroPython));
    assert!(
        app.browser.is_some(),
        "a scan needs somewhere to land its result"
    );
}

#[test]
fn applying_leaves_every_other_byte_of_the_file_alone() {
    let dir = TempDir::new("surgical");
    std::fs::write(
        dir.file(),
        "# hand-written, and committed\n\
         project_type = \"zephyr\"\n\
         \n\
         [future]\n\
         unknown = \"kept\"\n\
         \n\
         [[variant]]\n\
         name = \"sim\"\n\
         board = \"native_sim/native/64\"\n\
         \n\
         [[variant]]\n\
         name = \"board\"\n",
    )
    .unwrap();

    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    type_into(
        &mut app,
        ProjectConfigRow::ZephyrWorkspace,
        "/opt/zephyrproject",
    );
    apply(&mut app);

    let text = dir.text();
    assert!(
        text.starts_with("# hand-written, and committed\nproject_type = \"zephyr\"\n"),
        "the comment and the top-level key keep their place:\n{text}"
    );
    assert!(text.contains("[future]\nunknown = \"kept\"\n"), "{text}");
    assert!(text.contains("name = \"sim\"\n"), "{text}");
    assert!(text.contains("name = \"board\"\n"), "{text}");
    assert!(
        text.contains("[zephyr]\nworkspace = \"/opt/zephyrproject\"\n"),
        "the new section carries the answer:\n{text}"
    );

    // The variants are reported, and only reported.
    let panel = app.project_config.as_ref().unwrap();
    assert_eq!(panel.variants(), 2);
    assert!(panel.rows().contains(&ProjectConfigRow::Variants));
}

#[test]
fn del_removes_the_line_rather_than_blanking_it() {
    let dir = TempDir::new("clear");
    std::fs::write(
        dir.file(),
        "project_type = \"zephyr\"\n\n[zephyr]\nworkspace = \"/ws\"\nprojects = \"/apps\"\n",
    )
    .unwrap();

    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    go_to(&mut app, ProjectConfigRow::ZephyrWorkspace);
    app.handle(key(KeyCode::Delete));
    assert_eq!(
        app.project_config
            .as_ref()
            .unwrap()
            .pending_for(ProjectConfigRow::ZephyrWorkspace),
        Some(None),
        "a removal is a pending change like any other"
    );
    apply(&mut app);

    let text = dir.text();
    assert!(
        !text.contains("workspace"),
        "the key is gone, not empty:\n{text}"
    );
    assert!(
        text.contains("projects = \"/apps\""),
        "the rest stays:\n{text}"
    );
    assert!(
        text.contains("[zephyr]"),
        "the section header stays:\n{text}"
    );
}

#[test]
fn an_answer_typed_back_to_what_the_file_says_is_not_a_change() {
    let dir = TempDir::new("noop");
    std::fs::write(
        dir.file(),
        "project_type = \"zephyr\"\n\n[zephyr]\nworkspace = \"/ws\"\n",
    )
    .unwrap();
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);

    type_into(&mut app, ProjectConfigRow::ZephyrWorkspace, "/elsewhere");
    assert_eq!(app.project_config.as_ref().unwrap().change_count(), 1);
    type_into(&mut app, ProjectConfigRow::ZephyrWorkspace, "/ws");
    assert_eq!(
        app.project_config.as_ref().unwrap().change_count(),
        0,
        "a line that would write what is already there is not a change"
    );
}

#[test]
fn switching_backends_discards_that_backends_unapplied_answers_and_says_so() {
    let dir = TempDir::new("switch");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    pick_zephyr(&mut app);
    type_into(&mut app, ProjectConfigRow::ZephyrWorkspace, "/opt/ws");
    type_into(&mut app, ProjectConfigRow::ZephyrSdk, "/opt/sdk");

    app.handle(key(KeyCode::Home));
    app.handle(key(KeyCode::Left)); // MicroPython, the card before Zephyr

    let panel = app.project_config.as_ref().unwrap();
    assert_eq!(panel.chosen(), Some(BackendKind::MicroPython));
    assert_eq!(
        panel.change_count(),
        1,
        "only the choice is left --- what is pending is what is on screen"
    );
    let notice = panel.notice().expect("the discard is reported").text();
    assert!(
        notice.contains('2') && notice.contains("discarded"),
        "and it says how many: {notice}"
    );
}

#[test]
fn general_answers_survive_a_backend_switch() {
    // The theme is nobody's backend, so leaving one does not take it.
    let dir = TempDir::new("general");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    pick_zephyr(&mut app);
    go_to(&mut app, ProjectConfigRow::Icons);
    app.handle(key(KeyCode::Right));

    app.handle(key(KeyCode::Home));
    app.handle(key(KeyCode::Left)); // MicroPython
    assert!(
        app.project_config
            .as_ref()
            .unwrap()
            .pending_for(ProjectConfigRow::Icons)
            .is_some(),
        "a General answer belongs to the project, not to the backend"
    );
}

#[test]
fn the_theme_and_the_icons_preview_live_and_persist_only_on_apply() {
    let dir = TempDir::new("preview");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    let before = app.icon_set();

    go_to(&mut app, ProjectConfigRow::Icons);
    app.handle(key(KeyCode::Right)); // unicode, which is already the default
    app.handle(key(KeyCode::Right)); // nerd
    assert_eq!(
        app.project_config
            .as_ref()
            .unwrap()
            .value(ProjectConfigRow::Icons)
            .as_deref(),
        Some("nerd")
    );
    assert_ne!(
        app.icon_set(),
        before,
        "the window previews the answer live"
    );
    assert!(
        dir.user_text().is_empty(),
        "and nothing has been written for it:\n{}",
        dir.user_text()
    );

    // Discarding restores it for free: the preview was read off the panel,
    // never committed to the session.
    app.handle(key(KeyCode::Esc));
    app.handle(key(KeyCode::Char('y')));
    assert_eq!(app.icon_set(), before);
}

#[test]
fn applying_a_general_answer_writes_the_user_config() {
    let dir = TempDir::new("ui-keys");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    go_to(&mut app, ProjectConfigRow::Icons);
    app.handle(key(KeyCode::Right)); // unicode
    apply(&mut app);

    let text = dir.user_text();
    assert!(text.contains("[ui]"), "{text}");
    assert!(text.contains("icons = \"unicode\""), "{text}");
    assert!(
        !dir.file().exists(),
        "an app preference is not the project's business"
    );
}

#[test]
fn leaving_with_changes_asks_first_and_a_no_keeps_them() {
    let dir = TempDir::new("escape");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    pick_zephyr(&mut app);

    app.handle(key(KeyCode::Esc));
    assert!(
        matches!(app.overlay, Some(Overlay::ConfirmDiscardConfig { .. })),
        "there is something to lose: {:?}",
        app.overlay
    );
    app.handle(key(KeyCode::Char('n')));
    assert_eq!(app.overlay, Some(Overlay::ProjectConfig));
    assert_eq!(app.project_config.as_ref().unwrap().change_count(), 1);

    app.handle(key(KeyCode::Esc));
    app.handle(key(KeyCode::Char('y')));
    assert_eq!(app.overlay, None);
    assert!(!dir.file().exists());
}

#[test]
fn the_leave_dialog_defaults_to_keeping_the_work_and_can_apply_it() {
    let dir = TempDir::new("leave-apply");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    pick_zephyr(&mut app);

    app.handle(key(KeyCode::Esc));
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::ConfirmDiscardConfig { selected: 0 })
        ),
        "the choice that loses nothing is the default: {:?}",
        app.overlay
    );

    // Enter on the default: back to the window, nothing dropped.
    app.handle(key(KeyCode::Enter));
    assert_eq!(app.overlay, Some(Overlay::ProjectConfig));
    assert_eq!(app.project_config.as_ref().unwrap().change_count(), 1);

    // The third answer the Yes/No shape never had: write, then leave.
    app.handle(key(KeyCode::Esc));
    app.handle(key(KeyCode::Char('a')));
    assert_eq!(app.overlay, None);
    assert!(
        dir.text().contains("project_type = \"zephyr\""),
        "applied, not discarded: {}",
        dir.text()
    );
    assert!(app.project_config.is_none(), "and the window is closed");
}

#[test]
fn the_leave_dialog_walks_three_buttons_and_wraps() {
    let dir = TempDir::new("leave-walk");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    pick_zephyr(&mut app);

    app.handle(key(KeyCode::Esc));
    app.handle(key(KeyCode::Right));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmDiscardConfig { selected: 1 })
    ));
    app.handle(key(KeyCode::Right));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmDiscardConfig { selected: 2 })
    ));
    app.handle(key(KeyCode::Right));
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::ConfirmDiscardConfig { selected: 0 })
        ),
        "the walk wraps: {:?}",
        app.overlay
    );
    // Discard is still one keypress away from anywhere in the walk.
    app.handle(key(KeyCode::Char('y')));
    assert_eq!(app.overlay, None);
    assert!(!dir.file().exists());
}

#[test]
fn the_leave_dialog_lists_what_is_at_stake() {
    let dir = TempDir::new("leave-list");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    pick_zephyr(&mut app);
    type_into(
        &mut app,
        ProjectConfigRow::ZephyrBoard,
        "native_sim/native/64",
    );
    app.handle(key(KeyCode::Esc));

    let frame = common::render(&mut app, 110, 38);
    for expected in [
        "Leave without applying?",
        "project_type = \"zephyr\"",
        "board = \"native_sim/native/64\"",
        "Keep editing",
        "Apply and close",
        "Discard and close",
    ] {
        assert!(
            frame.contains(expected),
            "the dialog names what is at stake ({expected:?}):\n{frame}"
        );
    }
}

#[test]
fn leaving_an_unanswered_directory_goes_back_to_the_project_list() {
    let dir = TempDir::new("home").into_board_module();
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();

    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
    assert!(
        app.switch_requested(),
        "nothing behind this window, so leaving it leaves the session"
    );
    assert!(!dir.file().exists(), "and nothing was written");
    assert!(dir.registry().is_empty(), "nothing was recorded");
}

#[test]
fn leaving_an_answered_directory_asks_the_environments_own_question() {
    let dir = TempDir::new("chain").into_board_module();
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();
    pick_zephyr(&mut app);
    apply(&mut app);

    app.handle(key(KeyCode::Esc));
    assert!(
        matches!(
            app.overlay,
            Some(Overlay::DirPicker {
                purpose: chiptui::workspace::DirPurpose::Installation,
                ..
            })
        ),
        "the installation question rides the way out: {:?}",
        app.overlay
    );
    assert!(!app.switch_requested(), "the project resolved, so it stays");
}

#[test]
fn a_deliberately_opened_window_never_leaves_the_session() {
    let dir = TempDir::new("deliberate");
    std::fs::write(dir.file(), "project_type = \"micropython\"\n").unwrap();
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);

    app.handle(key(KeyCode::Esc));
    assert_eq!(app.overlay, None);
    assert!(!app.switch_requested());
}

#[test]
fn the_comma_opens_the_window_from_the_dashboard() {
    let dir = TempDir::new("comma");
    std::fs::write(dir.file(), "project_type = \"zephyr\"\n").unwrap();
    let mut app = dir.app();
    app.bootstrap();

    // Two spellings of one shortcut: the chord where the terminal can send
    // it, and the bare comma everywhere else.
    app.handle(key(KeyCode::Char(',')));
    assert_eq!(app.overlay, Some(Overlay::ProjectConfig));
    app.handle(key(KeyCode::Esc));
    app.handle(common::ctrl(','));
    assert_eq!(app.overlay, Some(Overlay::ProjectConfig));
}

#[test]
fn a_project_file_board_outranks_the_registry_entry() {
    let dir = TempDir::new("board");
    std::fs::write(
        dir.file(),
        "project_type = \"zephyr\"\n\n[zephyr]\nboard = \"ttgo_t_display_s3/esp32s3/procpu\"\n",
    )
    .unwrap();
    let mut entry = settings::ProjectEntry::new(&dir.path, BackendKind::Zephyr);
    entry.board = Some("nrf52840dk/nrf52840".into());
    settings::record_project(&settings::user_config_path(&dir.config_dir()), entry).unwrap();

    let mut app = dir.app();
    app.bootstrap();
    app.maybe_scan_devices();

    let board = app.build.as_ref().unwrap().board.as_ref().unwrap();
    assert_eq!(board.name, "ttgo_t_display_s3/esp32s3/procpu");
    assert_eq!(board.origin, chiptui::build::BoardOrigin::ProjectFile);
    assert_eq!(board.origin.label(), "chiptui.toml");
}

#[test]
fn a_project_file_micropython_projects_folder_outranks_the_user_config() {
    let dir = TempDir::new("mpy");
    let mine = dir.path.join("mine");
    let theirs = dir.path.join("theirs");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();
    settings::save_mpy_projects(&settings::user_config_path(&dir.config_dir()), &theirs).unwrap();
    std::fs::write(
        dir.file(),
        format!(
            "project_type = \"micropython\"\n\n[micropython]\nprojects = \"{}\"\n",
            mine.display()
        ),
    )
    .unwrap();

    let mut app = dir.app();
    app.bootstrap();
    app.maybe_scan_devices();
    assert_eq!(
        app.mpy_projects.as_deref(),
        Some(mine.as_path()),
        "the project's own answer wins, the same rule [zephyr] follows"
    );
}

#[test]
fn a_confidently_detected_project_is_never_asked() {
    let dir = TempDir::new("confident");
    std::fs::write(dir.path.join("boot.py"), "").unwrap();
    std::fs::write(dir.path.join("main.py"), "").unwrap();

    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();

    assert_eq!(app.overlay, None, "MicroPython was detected confidently");
    assert_eq!(app.manager.selected_kind(), Some(BackendKind::MicroPython));
    assert!(!dir.file().exists(), "and nothing is written for it");
}

#[test]
fn a_project_the_registry_already_names_is_never_asked() {
    let dir = TempDir::new("registered");
    settings::record_project(
        &settings::user_config_path(&dir.config_dir()),
        settings::ProjectEntry::new(&dir.path, BackendKind::Zephyr),
    )
    .unwrap();
    std::fs::write(dir.path.join("notes.txt"), "hi").unwrap();

    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();

    assert_eq!(app.overlay, None, "the registry already answered");
    assert_eq!(app.manager.selected_kind(), Some(BackendKind::Zephyr));
}

#[test]
fn re_detecting_asks_again_until_resolved() {
    let dir = TempDir::new("re-detect").into_board_module();
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();
    app.overlay = None;
    app.project_config = None;

    app.handle(key(KeyCode::Char('r')));
    assert_eq!(app.overlay, Some(Overlay::ProjectConfig));
}

/// Render-pinned: the clicks land on the *drawn* geometry, found in the
/// frame, because byte offsets are not columns (the borders are multi-byte).
#[test]
fn sections_draw_a_rule_a_count_and_the_backends_edge() {
    let dir = TempDir::new("sections");
    std::fs::write(
        dir.file(),
        "project_type = \"zephyr\"\n\n[zephyr]\nboard = \"qemu_x86\"\n",
    )
    .unwrap();
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);

    let frame = common::render(&mut app, 110, 38);
    let heading = |name: &str| {
        frame
            .lines()
            .find(|line| line.contains(name) && line.contains('/'))
            .unwrap_or_else(|| panic!("the {name} heading is not drawn:\n{frame}"))
    };
    let general = heading("General");
    assert!(
        general.contains("5/5") && general.contains('─'),
        "General is a divider with its answered count:\n{general}"
    );
    let zephyr = heading("Zephyr");
    assert!(
        zephyr.contains("1/6"),
        "one of the six Zephyr rows is answered:\n{zephyr}"
    );

    let governed = frame
        .lines()
        .find(|line| line.contains("Target board") && line.contains("qemu_x86"))
        .expect("the board row is drawn");
    assert!(
        governed.contains('▎'),
        "a row the chosen backend owns carries its edge:\n{governed}"
    );
    let plain = frame
        .lines()
        .find(|line| line.contains("Color theme"))
        .expect("a General row is drawn");
    assert!(
        !plain.contains('▎'),
        "General stays unmarked --- the boundary is what the edge is for:\n{plain}"
    );
}

#[test]
fn the_details_pane_names_the_key_the_winner_and_the_pending_line() {
    let dir = TempDir::new("details");
    std::fs::write(
        dir.file(),
        "project_type = \"zephyr\"\n\n[zephyr]\nboard = \"qemu_x86\"\n",
    )
    .unwrap();
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);
    go_to(&mut app, ProjectConfigRow::ZephyrBoard);

    let frame = common::render(&mut app, 110, 38);
    for expected in [
        "Details",
        "[zephyr] board",
        "Current",
        "▸ chiptui.toml",
        "qemu_x86",
        // The precedence the "Current" block shows per row, stated once.
        "more specific wins",
    ] {
        assert!(frame.contains(expected), "missing {expected:?}:\n{frame}");
    }

    type_into(&mut app, ProjectConfigRow::ZephyrBoard, "native_sim");
    let frame = common::render(&mut app, 110, 38);
    for expected in ["Will write", "board = \"native_sim\""] {
        assert!(frame.contains(expected), "missing {expected:?}:\n{frame}");
    }
}

#[test]
fn the_details_pane_lists_every_theme_and_scrolls() {
    let dir = TempDir::new("themes");
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);

    go_to(&mut app, ProjectConfigRow::Theme);
    let frame = common::render(&mut app, 110, 38);
    let options = frame.matches('○').count();
    assert!(
        options >= 10,
        "the theme list is drawn whole, not summarised ({options} visible):\n{frame}"
    );
    assert!(
        !frame.contains("cycles them"),
        "no summary stands in for the list:\n{frame}"
    );

    // `Tab` hands the pane the keyboard: the arrows scroll it, a page
    // moves by the rows the frame actually drew, and a row move starts
    // the next row's document from its top.
    let viewport = app.config_details_viewport;
    assert!(viewport > 0, "the renderer published the pane's height");
    app.handle(key(KeyCode::Tab));
    app.handle(key(KeyCode::Down));
    app.handle(key(KeyCode::Down));
    assert_eq!(app.project_config.as_ref().unwrap().details_scroll(), 2);
    app.handle(key(KeyCode::PageDown));
    assert_eq!(
        app.project_config.as_ref().unwrap().details_scroll(),
        2 + viewport
    );
    app.handle(key(KeyCode::PageUp));
    assert_eq!(app.project_config.as_ref().unwrap().details_scroll(), 2);
    app.handle(key(KeyCode::Char('j')));
    assert_eq!(
        app.project_config.as_ref().unwrap().details_scroll(),
        0,
        "a new row is a new document, from its top"
    );
    assert_eq!(
        app.project_config.as_ref().unwrap().selected(),
        Some(ProjectConfigRow::Icons),
        "j/k keep walking rows with the details focused"
    );
}

#[test]
fn rows_carry_a_state_mark_and_pending_ones_the_transition() {
    let dir = TempDir::new("marks");
    std::fs::write(
        dir.file(),
        "project_type = \"zephyr\"\n\n[zephyr]\nboard = \"qemu_x86\"\n",
    )
    .unwrap();
    let mut app = dir.app();
    app.bootstrap();
    open(&mut app);

    let frame = common::render(&mut app, 110, 38);
    // The details pane repeats the selected row's label as its heading,
    // so a row is found by its label *and* its value together.
    fn line<'a>(frame: &'a str, needle: &str, value: &str) -> &'a str {
        frame
            .lines()
            .find(|line| line.contains(needle) && line.contains(value))
            .unwrap_or_else(move || panic!("{needle} = {value} is not drawn:\n{frame}"))
    }
    let board = line(&frame, "Target board", "qemu_x86");
    assert!(
        board.contains('✓'),
        "a saved answer carries its mark and its value:\n{board}"
    );
    let theme = line(&frame, "Color theme", "default");
    assert!(
        theme.contains('←'),
        "an inherited answer says so, and from where:\n{theme}"
    );
    let shield = line(&frame, "Shield", "—");
    assert!(
        shield.contains('·'),
        "an unanswered row is marked, not blank:\n{shield}"
    );

    // Editing the board replaces the value in place with the transition,
    // not with a bare new word.
    type_into(&mut app, ProjectConfigRow::ZephyrBoard, "native_sim");
    let frame = common::render(&mut app, 110, 38);
    let board = line(&frame, "Target board", "qemu_x86 → native_sim");
    assert!(
        board.contains('●'),
        "a pending answer names what it replaces:\n{board}"
    );
}

#[test]
fn a_click_picks_a_card_selects_a_row_and_one_outside_closes_the_window() {
    let dir = TempDir::new("click");
    std::fs::write(dir.file(), "project_type = \"zephyr\"\n").unwrap();
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_scan_devices(); // the startup sequence's own order
    app.set_mouse_enabled(true);
    open(&mut app);

    let frame = common::render(&mut app, 110, 38);
    let (card_row, card_col) = frame
        .lines()
        .enumerate()
        .find_map(|(row, line)| line.find("MicroPython").map(|col| (row, col)))
        .expect("the cards are drawn");
    app.handle(AppEvent::Mouse(common::click(
        card_col as u16,
        card_row as u16,
    )));
    assert_eq!(
        app.project_config.as_ref().unwrap().chosen(),
        Some(BackendKind::MicroPython),
        "a card is a button: the click presses it"
    );

    let frame = common::render(&mut app, 110, 38);
    let row = frame
        .lines()
        .position(|line| line.contains(" Icon set "))
        .expect("the icons row is drawn") as u16;
    app.handle(AppEvent::Mouse(common::click(8, row)));
    assert_eq!(
        app.project_config.as_ref().unwrap().selected(),
        Some(ProjectConfigRow::Icons),
        "a row only selects --- the answer beside it needs a second gesture"
    );

    // Outside the box: the same `Esc` every other overlay answers a click
    // outside with, which here means the discard question.
    app.handle(AppEvent::Mouse(common::click(0, 0)));
    assert!(matches!(
        app.overlay,
        Some(Overlay::ConfirmDiscardConfig { .. })
    ));
}

/// Resizing the terminal small must never take the process down: the
/// details pane's value budget is `saturating_sub(22)`, which reaches zero
/// on a terminal narrower than the list column plus that margin, and
/// `shorten_tail` used to underflow its `max_chars - 1` on exactly that
/// row --- a silent exit 101, the panic message swallowed by the
/// alternate screen.
#[test]
fn shrinking_the_terminal_keeps_the_window_alive() {
    let dir = TempDir::new("resize").into_board_module();
    let mut app = dir.app();
    app.bootstrap();
    app.maybe_open_project_config();
    pick_zephyr(&mut app);

    // Walk every row: any of them may be the one whose fallback value is
    // on screen when the resize lands.
    for _ in 0..60 {
        for (width, height) in [(2, 10), (40, 24), (60, 30), (82, 30), (83, 40), (100, 32)] {
            common::render(&mut app, width, height);
        }
        let panel = app.project_config.as_ref().unwrap();
        let last = matches!(
            panel.cursor(),
            Cursor::Row(index) if index + 1 >= panel.rows().len()
        );
        if last {
            break;
        }
        app.handle(key(KeyCode::Down));
    }
}
