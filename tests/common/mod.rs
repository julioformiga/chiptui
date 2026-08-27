//! Scaffolding shared by the integration tests.
//!
//! Every file under `tests/` is its own crate, so before this module each one
//! carried its own copy of the same handful of helpers: `key` in thirteen
//! files, `render` in eleven, `pump_until` in seven --- byte-identical apart
//! from `unwrap()` vs. `expect()` and whether the paths were spelled out or
//! imported. What lives here is only the part that was genuinely the same
//! everywhere; a helper whose variants differed in *behaviour* (the docs-aware
//! `pump_until`, the five shapes of `zephyr_app`) deliberately stays local to
//! the file that needs it, because collapsing those would change what the
//! tests do rather than how they are written.
//!
//! Compiled into each test binary separately, so most of it is unused in any
//! given one --- hence the blanket allow.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chiptui::app::App;
use chiptui::event::AppEvent;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

/// The absolute path of a fake tool in `tests/fixtures/bin/`.
///
/// Absolute on purpose: nothing here mutates `PATH`, so the tests stay
/// parallel-safe (`CLAUDE.md`, "Testing").
pub fn fake(tool: &str) -> String {
    format!("{}/tests/fixtures/bin/{tool}", env!("CARGO_MANIFEST_DIR"))
}

pub fn fake_mpremote() -> String {
    fake("mpremote")
}

pub fn fake_mpremote_second_board() -> String {
    fake("mpremote-second-board")
}

pub fn fake_curl() -> String {
    fake("curl")
}

/// A bare keypress, no modifiers.
pub fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A keypress carrying `modifiers`.
pub fn key_event(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, modifiers))
}

/// `ctrl+<c>`, the dashboard's chord family.
pub fn ctrl(c: char) -> AppEvent {
    key_event(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// A left click at `(column, row)`.
///
/// Columns are *drawn* columns, never byte offsets into the frame's lines ---
/// the borders are multi-byte (`CLAUDE.md`, "Click tests are render-pinned").
pub fn click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// One frame of the dashboard at `width` x `height`, as text.
pub fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| chiptui::ui::draw(frame, app))
        .expect("draw succeeds");
    terminal.backend().to_string()
}

/// Drives the app until `done` holds or `secs` run out, draining process
/// events and ticking between checks.
///
/// The tick matters: the deferred device queries are tick-polled, so a loop
/// without it never advances them.
pub fn pump_until(app: &mut App, mut done: impl FnMut(&App) -> bool, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        if done(app) {
            return true;
        }
        app.handle(AppEvent::Tick);
        std::thread::sleep(Duration::from_millis(5));
    }
    done(app)
}

/// Drains process events until `busy` goes false, then asserts it did.
///
/// `what` names the thing that was expected to finish, so a timeout reads as
/// "esptool command never completed" rather than a bare assertion.
pub fn settle_while(app: &mut App, busy: impl Fn(&App) -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while busy(app) && Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!busy(app), "{what} never completed");
}

/// An app whose config reads come from a scratch home.
///
/// `App::new` reads `[ui] icons` out of `$HOME`, so without this the frames
/// assert against whatever the developer happens to have configured. `prefix`
/// keeps one test binary's scratch homes from colliding with another's.
pub fn hermetic_app(root: impl Into<PathBuf>, prefix: &str) -> App {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut app = App::new(root);
    let home = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    app.set_home_dir(home);
    app
}

/// The Environment pane's way in: the shortcuts overlay (`ctrl+k`), then its
/// `e` letter.
pub fn enter_project_pane(app: &mut App) {
    app.handle(ctrl('k'));
    app.handle(key(KeyCode::Char('e')));
}

/// Whether any log entry contains `needle`.
pub fn log_mentions(app: &App, needle: &str) -> bool {
    app.logs
        .visible(usize::MAX)
        .any(|entry| entry.message.contains(needle))
}
