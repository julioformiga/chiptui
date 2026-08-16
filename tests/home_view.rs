//! The home screen's rendering (`SPEC.md` §7/§11), against ratatui's
//! `TestBackend` --- no terminal required, like `tests/ui_render.rs`.

use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use chiptui::backend::BackendKind;
use chiptui::home::HomeScreen;
use chiptui::settings::{self, ProjectEntry};

struct Fixture {
    home: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let home =
            std::env::temp_dir().join(format!("chiptui-home-view-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".config")).unwrap();
        Self { home }
    }

    fn config_dir(&self) -> PathBuf {
        self.home.join(".config")
    }

    fn record(&self, name: &str, backend: BackendKind) -> PathBuf {
        let dir = self.home.join("apps").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        settings::record_project(
            &settings::user_config_path(&self.config_dir()),
            ProjectEntry::new(&dir, backend).opened_now(),
        )
        .unwrap();
        dir
    }

    fn screen(&self) -> HomeScreen {
        HomeScreen::new(&self.config_dir(), &self.home)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn render(screen: &HomeScreen, width: u16, height: u16) -> String {
    let theme = ratatui_themes::ThemeName::TokyoNight.palette();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| chiptui::ui::home::draw(frame, screen, theme))
        .expect("draw succeeds");
    terminal.backend().to_string()
}

fn press(screen: &mut HomeScreen, code: KeyCode) {
    screen.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn the_list_shows_each_project_with_its_backend_and_path() {
    let fixture = Fixture::new("list");
    fixture.record("blinky", BackendKind::Zephyr);
    fixture.record("sensor-node", BackendKind::MicroPython);

    let frame = render(&fixture.screen(), 100, 20);

    assert!(frame.contains("ChipTUI"), "{frame}");
    assert!(frame.contains("New project"), "{frame}");
    assert!(frame.contains("Zephyr"), "{frame}");
    assert!(frame.contains("MicroPython"), "{frame}");
    assert!(frame.contains("blinky"), "{frame}");
    assert!(
        frame.contains("~/apps/sensor-node"),
        "paths abbreviate:\n{frame}"
    );
    assert!(
        frame.contains("enter open"),
        "the footer names the keys:\n{frame}"
    );
}

#[test]
fn an_empty_registry_says_so_instead_of_showing_a_blank_panel() {
    let fixture = Fixture::new("empty");
    let frame = render(&fixture.screen(), 90, 14);
    assert!(frame.contains("No projects yet"), "{frame}");
    assert!(frame.contains("New project"), "{frame}");
}

#[test]
fn the_search_field_shows_what_was_typed_and_filters_the_frame() {
    let fixture = Fixture::new("search");
    fixture.record("blinky", BackendKind::Zephyr);
    fixture.record("sensor-node", BackendKind::MicroPython);

    // Matching is over name, path and backend, so the term is picked to
    // appear in none of `blinky`'s three (the temp path is shared).
    let mut screen = fixture.screen();
    for c in "node".chars() {
        press(&mut screen, KeyCode::Char(c));
    }
    let frame = render(&screen, 100, 20);

    assert!(frame.contains("node"), "the query is echoed:\n{frame}");
    assert!(frame.contains("sensor-node"), "{frame}");
    assert!(
        !frame.contains("blinky"),
        "the filter hides the rest:\n{frame}"
    );
}

#[test]
fn a_project_row_is_tinted_with_its_backend_colour() {
    use ratatui::style::Color;

    let fixture = Fixture::new("tint");
    fixture.record("blinky", BackendKind::Zephyr);

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
    let screen = fixture.screen();
    let theme = ratatui_themes::ThemeName::TokyoNight.palette();
    terminal
        .draw(|frame| chiptui::ui::home::draw(frame, &screen, theme))
        .expect("draw succeeds");
    let buffer = terminal.backend().buffer().clone();

    let row = (0..20)
        .find(|y| {
            (0..100).any(|x| buffer[(x, *y)].symbol().contains('b'))
                && (0..100).any(|x| buffer[(x, *y)].bg == Color::Indexed(17))
                || (0..100).any(|x| buffer[(x, *y)].bg == Color::Indexed(25))
        })
        .expect("the Zephyr row should carry a blue tint");

    let tinted = (0..100)
        .filter(|x| {
            matches!(
                buffer[(*x, row)].bg,
                Color::Indexed(17) | Color::Indexed(25)
            )
        })
        .count();
    assert!(
        tinted > 50,
        "the tint should span the row, only {tinted} cells carried it"
    );
}

#[test]
fn creating_a_project_draws_the_folder_picker_then_the_name_prompt() {
    let fixture = Fixture::new("create");

    let mut screen = fixture.screen();
    press(&mut screen, KeyCode::Enter); // the create row
    let picker = render(&screen, 100, 24);
    assert!(
        picker.contains("Where should the project folder go?"),
        "{picker}"
    );
    assert!(picker.contains("put it in this directory"), "{picker}");

    press(&mut screen, KeyCode::Enter); // accept the starting folder
    press(&mut screen, KeyCode::Char('x'));
    let name = render(&screen, 100, 24);
    assert!(name.contains("Name the project"), "{name}");
    assert!(
        name.contains("the backend is asked next"),
        "the flow says what happens after:\n{name}"
    );

    let _ = std::fs::remove_dir_all(fixture.home.join("x"));
}

#[test]
fn forgetting_a_project_explains_that_the_folder_stays() {
    let fixture = Fixture::new("forget");
    let dir = fixture.record("blinky", BackendKind::Zephyr);

    let mut screen = fixture.screen();
    press(&mut screen, KeyCode::Down);
    press(&mut screen, KeyCode::Delete);
    let frame = render(&screen, 100, 20);

    assert!(frame.contains("Remove from the list?"), "{frame}");
    assert!(frame.contains("stay exactly where they are"), "{frame}");
    assert!(dir.is_dir());
}

#[test]
fn a_narrow_terminal_still_renders_every_column() {
    let fixture = Fixture::new("narrow");
    fixture.record("blinky", BackendKind::Zephyr);

    let frame = render(&fixture.screen(), 60, 10);
    assert!(frame.contains("Zephyr"), "{frame}");
    let row = frame
        .lines()
        .find(|line| line.contains("blinky"))
        .expect("the project row is drawn");
    assert!(
        row.contains('…'),
        "the path column shrinks instead of pushing the row over the edge:\n{frame}"
    );
    assert!(
        row.trim_end().ends_with('│') || row.contains("│  "),
        "the row stays inside the panel:\n{frame}"
    );
}

/// The path column keeps the project's own folder when it cannot fit.
#[test]
fn a_long_path_loses_its_head_not_its_tail() {
    let fixture = Fixture::new("long");
    let deep = fixture
        .home
        .join("some/rather/deeply/nested/place/for/projects/blinky");
    std::fs::create_dir_all(&deep).unwrap();
    settings::record_project(
        &settings::user_config_path(&fixture.config_dir()),
        ProjectEntry::new(&deep, BackendKind::Zephyr).opened_now(),
    )
    .unwrap();

    let frame = render(&fixture.screen(), 70, 12);
    assert!(frame.contains("blinky"), "{frame}");
    assert!(frame.contains('…'), "the head is elided:\n{frame}");
}
