//! Leaving a project for the home screen (`shift+P`).
//!
//! The dashboard cannot draw the home screen itself --- the binary owns that
//! loop --- so what `App` must get right is the request: immediate when
//! nothing is running, confirmed (and cancellable) when something is.

use std::time::Duration;

use chiptui::app::{App, Overlay};
use chiptui::process::Command;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::key;

fn app() -> App {
    let dir = std::env::temp_dir().join(format!("chiptui-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = App::new(&dir);
    // Answering nothing here, but a redirected home keeps any config write
    // out of the developer's real one.
    app.set_home_dir(dir.join("home"));
    app.bootstrap();
    app
}

/// `tests/fixtures/bin/slow` sleeps, so it is still running when asserted on.
fn slow() -> Command {
    Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/bin/slow"
    ))
}

#[test]
fn with_nothing_running_the_request_is_immediate() {
    let mut app = app();
    app.handle(key(KeyCode::Char('P')));

    assert_eq!(app.overlay, None, "nothing to warn about");
    assert!(app.switch_requested());
    assert!(app.take_switch_request());
    assert!(!app.switch_requested(), "the request is consumed once");
}

#[test]
fn a_running_command_is_named_before_the_project_is_left() {
    let mut app = app();
    app.processes.spawn(slow(), Duration::from_secs(30));
    assert_eq!(app.running_commands(), 1);

    app.handle(key(KeyCode::Char('P')));
    assert_eq!(
        app.overlay,
        Some(Overlay::ConfirmSwitchProject { confirm: false }),
        "the default is No: leaving cancels the command"
    );
    assert!(!app.switch_requested());

    // Accepting needs the explicit "yes".
    app.handle(key(KeyCode::Char('y')));
    assert_eq!(app.overlay, None);
    assert!(app.switch_requested());
}

#[test]
fn declining_keeps_the_project_open() {
    let mut app = app();
    app.processes.spawn(slow(), Duration::from_secs(30));

    app.handle(key(KeyCode::Char('P')));
    app.handle(key(KeyCode::Esc));

    assert_eq!(app.overlay, None);
    assert!(!app.switch_requested(), "esc cancels the switch");
    assert!(!app.should_quit(), "and does not quit either");
}
