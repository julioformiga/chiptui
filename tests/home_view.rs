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
fn the_none_icon_set_hides_the_backend_marks() {
    let fixture = Fixture::new("icons-none");
    // The `[ui] icons` answer is read once in `HomeScreen::new`, the same
    // startup read the dashboard's `App::new` does.
    std::fs::create_dir_all(fixture.config_dir().join("chiptui")).unwrap();
    std::fs::write(
        settings::user_config_path(&fixture.config_dir()),
        "[ui]\nicons = \"none\"\n",
    )
    .unwrap();
    fixture.record("blinky", BackendKind::Zephyr);
    fixture.record("sensor-node", BackendKind::MicroPython);

    let frame = render(&fixture.screen(), 100, 20);

    assert!(
        !frame.contains('🔷') && !frame.contains('🐍'),
        "the backend marks are decoration the none set drops:\n{frame}"
    );
    assert!(
        frame.contains("Zephyr") && frame.contains("MicroPython"),
        "the names carry the distinction on their own:\n{frame}"
    );
}

#[test]
fn the_nerd_set_gives_both_backends_a_single_width_mark() {
    let fixture = Fixture::new("icons-nerd");
    std::fs::create_dir_all(fixture.config_dir().join("chiptui")).unwrap();
    std::fs::write(
        settings::user_config_path(&fixture.config_dir()),
        "[ui]\nicons = \"nerd\"\n",
    )
    .unwrap();
    fixture.record("blinky", BackendKind::Zephyr);
    fixture.record("sensor-node", BackendKind::MicroPython);

    let frame = render(&fixture.screen(), 100, 20);

    assert!(
        frame.contains('\u{E73C}'),
        "MicroPython gets the Python logo:\n{frame}"
    );
    assert!(
        !frame.contains('🐍'),
        "the MicroPython emoji steps aside for it:\n{frame}"
    );
    assert!(
        frame.contains('◆'),
        "Zephyr trades its two-cell emoji for the header's single-width diamond:\n{frame}"
    );
    assert!(
        !frame.contains('🔷'),
        "the Zephyr emoji steps aside for it:\n{frame}"
    );
}

/// The buffer cell `needle` starts at, scanning rows top to bottom --- a
/// true cell coordinate, not a char offset, which is the whole point:
/// the emoji marks are one char over two cells, and the alignment the
/// icon column promises is in cells.
fn cell_x(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<u16> {
    let chars: Vec<char> = needle.chars().collect();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let hit = (0..chars.len()).all(|i| {
                let cx = x + i as u16;
                cx < buffer.area.width && buffer[(cx, y)].symbol() == chars[i].to_string()
            });
            if hit {
                return Some(x);
            }
        }
    }
    None
}

#[test]
fn the_icon_column_keeps_both_backends_rows_aligned_under_the_nerd_set() {
    let fixture = Fixture::new("icons-nerd-align");
    std::fs::create_dir_all(fixture.config_dir().join("chiptui")).unwrap();
    std::fs::write(
        settings::user_config_path(&fixture.config_dir()),
        "[ui]\nicons = \"nerd\"\n",
    )
    .unwrap();
    fixture.record("blinky", BackendKind::Zephyr);
    fixture.record("sensor-node", BackendKind::MicroPython);

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
    let screen = fixture.screen();
    let theme = ratatui_themes::ThemeName::TokyoNight.palette();
    terminal
        .draw(|frame| chiptui::ui::home::draw(frame, &screen, theme))
        .expect("draw succeeds");
    let buffer = terminal.backend().buffer().clone();

    // The Python logo is one cell wide where Zephyr's emoji mark is
    // two; `icon_column` pads the difference so every text column
    // after the mark starts at the same cell on both kinds of row.
    assert_eq!(
        cell_x(&buffer, "Zephyr").expect("the Zephyr label is drawn"),
        cell_x(&buffer, "MicroPython").expect("the MicroPython label is drawn"),
        "the backend column starts at the same cell on every row"
    );
    assert_eq!(
        cell_x(&buffer, "blinky").expect("the Zephyr row's name"),
        cell_x(&buffer, "sensor-node").expect("the MicroPython row's name"),
        "the name column starts at the same cell on every row"
    );

    // And the marks themselves line up: both are single-width under the
    // Nerd set (the diamond and the logo), so `icon_column` centers
    // each identically over the column's two-cell glyph slot rather
    // than one hugging the left edge and the other the middle.
    let diamond = cell_x(&buffer, "◆").expect("the Zephyr mark is drawn");
    let logo = cell_x(&buffer, "\u{E73C}").expect("the Python logo is drawn");
    assert_eq!(
        logo, diamond,
        "both single-width marks center at the same cell"
    );
}

/// The first cell whose symbol equals `needle` --- for asserting on the
/// *style* a mark was drawn with, not just that it appears.
fn cell_of<'a>(
    buffer: &'a ratatui::buffer::Buffer,
    needle: &str,
) -> Option<&'a ratatui::buffer::Cell> {
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            if buffer[(x, y)].symbol() == needle {
                return Some(&buffer[(x, y)]);
            }
        }
    }
    None
}

#[test]
fn the_backend_marks_carry_their_backends_accent_color() {
    let fixture = Fixture::new("icons-nerd-color");
    std::fs::create_dir_all(fixture.config_dir().join("chiptui")).unwrap();
    std::fs::write(
        settings::user_config_path(&fixture.config_dir()),
        "[ui]\nicons = \"nerd\"\n",
    )
    .unwrap();
    fixture.record("blinky", BackendKind::Zephyr);
    fixture.record("sensor-node", BackendKind::MicroPython);

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
    let screen = fixture.screen();
    let theme = ratatui_themes::ThemeName::TokyoNight.palette();
    terminal
        .draw(|frame| chiptui::ui::home::draw(frame, &screen, theme))
        .expect("draw succeeds");
    let buffer = terminal.backend().buffer().clone();

    // A single-width Nerd glyph has no color of its own the way an emoji
    // does; it takes the backend's accent --- the same blue the Zephyr
    // name beside it is drawn in, and green for the Python logo.
    let diamond = cell_of(&buffer, "◆").expect("the Zephyr mark is drawn");
    assert_eq!(
        diamond.fg,
        theme.info,
        "the Zephyr mark rides the theme's info blue:\n{}",
        terminal.backend()
    );
    let logo = cell_of(&buffer, "\u{E73C}").expect("the Python logo is drawn");
    assert_eq!(
        logo.fg,
        theme.success,
        "the MicroPython mark rides the theme's success green:\n{}",
        terminal.backend()
    );
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
        frame.contains("del forget"),
        "the footer names the keys a user cannot guess:\n{frame}"
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
    let fixture = Fixture::new("tint");
    fixture.record("blinky", BackendKind::Zephyr);

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
    let screen = fixture.screen();
    let theme = ratatui_themes::ThemeName::TokyoNight.palette();
    let zephyr = BackendKind::Zephyr.palette(theme);
    terminal
        .draw(|frame| chiptui::ui::home::draw(frame, &screen, theme))
        .expect("draw succeeds");
    let buffer = terminal.backend().buffer().clone();

    let row = (0..20)
        .find(|y| {
            (0..100).any(|x| buffer[(x, *y)].symbol().contains('b'))
                && (0..100).any(|x| {
                    let bg = buffer[(x, *y)].bg;
                    bg == zephyr.tint || bg == zephyr.tint_selected
                })
        })
        .expect("the Zephyr row should carry its theme-derived tint");

    let tinted = (0..100)
        .filter(|x| {
            let bg = buffer[(*x, row)].bg;
            bg == zephyr.tint || bg == zephyr.tint_selected
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
