//! The Terminal tab's shell session against a real PTY child.
//!
//! The unit tests drive synthetic `ProcessEvent`s; this one runs a real
//! `/bin/sh` through the app the way the event loop does --- spawn via the
//! tab, drain `ProcessManager` into `App::handle` until the child finishes
//! --- so the PTY arm, the `LineConsole` feed and the exit line are
//! exercised end to end (`AGENTS.md` §Testing: no board, just a process).

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::app::App;
use chiptui::event::AppEvent;
use chiptui::process::Command;

#[test]
fn the_terminal_tab_streams_a_real_pty_shell_to_its_exit_line() {
    let mut app = App::new("/nonexistent-project-dir");
    app.set_terminal_tool(
        Command::new("/bin/sh")
            .arg("-c")
            .arg("echo hi-from-the-shell"),
    );
    app.show_terminal_tab();
    assert!(app.terminal_process.is_some(), "the shell session started");

    // The event loop's drain, replayed: output becomes transcript, the
    // exit becomes the `[shell ...]` line and frees the keyboard.
    let deadline = Instant::now() + Duration::from_secs(20);
    while app.terminal_process.is_some() && Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(app.terminal_process.is_none(), "the shell exited");
    assert!(!app.is_terminal_active(), "the keyboard is free again");
    assert!(
        app.terminal_output
            .iter()
            .any(|line| line.contains("hi-from-the-shell")),
        "the transcript holds the shell's output: {:?}",
        app.terminal_output
    );
    assert_eq!(
        app.terminal_output.last().map(String::as_str),
        Some("[shell ok]"),
        "the exit line lands last: {:?}",
        app.terminal_output
    );
}
