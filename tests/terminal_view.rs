//! The Terminal tab's shell session against a real PTY child.
//!
//! The unit tests drive synthetic `ProcessEvent`s; these run a real
//! `/bin/sh` through the app the way the event loop does --- spawn via the
//! tab, drain `ProcessManager` into `App::handle` until the child finishes
//! --- so the raw PTY arm, the `vt100` feed and the exit line are exercised
//! end to end (`AGENTS.md` §Testing: no board, just a process).

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
    let screen = app.terminal.screen().contents();
    assert!(
        screen.contains("hi-from-the-shell"),
        "the grid holds the shell's output: {screen:?}"
    );
    assert!(
        screen.contains("[shell ok]"),
        "the exit line lands in the grid: {screen:?}"
    );
}

/// The emulator is a screen, not a log: `clear` (an `ED 2` plus a cursor
/// home) empties it. The old line-oriented console could only ever append,
/// so this is the case that proves the tab now models a terminal.
#[test]
fn clearing_the_screen_empties_the_grid() {
    let mut app = App::new("/nonexistent-project-dir");
    app.set_terminal_tool(
        Command::new("/bin/sh")
            .arg("-c")
            // Doubled backslashes: these escapes are the *shell's*, not
            // Rust's --- and `\033` in a Rust literal is a NUL byte
            // followed by "33", which truncates the argument at exec.
            .arg("printf 'before-the-clear\\n'; printf '\\033[2J\\033[H'; printf 'after\\n'"),
    );
    app.show_terminal_tab();

    let deadline = Instant::now() + Duration::from_secs(20);
    while app.terminal_process.is_some() && Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(app.terminal_process.is_none(), "the shell exited");

    let screen = app.terminal.screen().contents();
    assert!(
        !screen.contains("before-the-clear"),
        "the clear wiped the earlier output: {screen:?}"
    );
    assert!(
        screen.contains("after"),
        "what came after the clear stayed: {screen:?}"
    );
}

/// A multi-byte character split across the PTY reader's 1 KiB buffer must
/// survive. Decoding each chunk on its own --- which the `Output` path does
/// --- replaces the halves with U+FFFD, and a powerline separator (U+E0B0,
/// three bytes) is exactly the kind of character a shell prompt is made of.
#[test]
fn a_glyph_split_across_a_read_boundary_survives() {
    let mut app = App::new("/nonexistent-project-dir");
    // 1023 bytes of padding, so the three-byte separator starts one byte
    // before the boundary and finishes after it.
    app.set_terminal_tool(
        Command::new("/bin/sh")
            .arg("-c")
            .arg("printf 'x%.0s' $(seq 1023); printf '\\356\\202\\260'; printf '\\r\\n'"),
    );
    app.show_terminal_tab();

    let deadline = Instant::now() + Duration::from_secs(20);
    while app.terminal_process.is_some() && Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let screen = app.terminal.screen().contents();
    assert!(
        screen.contains('\u{e0b0}'),
        "the powerline separator survived the boundary: {screen:?}"
    );
    assert!(
        !screen.contains('\u{fffd}'),
        "nothing was replaced: {screen:?}"
    );
}

/// The whole point of the emulator: the shell's own colours reach the
/// screen, its powerline glyph arrives as one intact cell, and a segment
/// placed by cursor motion (`CSI n C`) lands at the column it asked for.
///
/// The old line-oriented console failed all three --- it dropped every SGR,
/// and its `CSI C` could not move past the end of the line it was editing.
#[test]
fn the_grid_keeps_the_shells_colours_glyph_and_column() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    let mut app = App::new("/nonexistent-project-dir");
    app.set_terminal_tool(Command::new(format!(
        "{}/tests/fixtures/bin/p10k-frame",
        env!("CARGO_MANIFEST_DIR")
    )));
    app.focus = chiptui::app::Focus::Logs;
    app.show_terminal_tab();
    let id = app.terminal_process.expect("the shell session started");

    // The fixture prints its frame and then holds, so wait for the frame
    // rather than for an exit.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !app.terminal.screen().contents().contains("12:34:56") && Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut terminal = Terminal::new(TestBackend::new(100, 32)).expect("test terminal");
    terminal
        .draw(|frame| chiptui::ui::draw(frame, &mut app))
        .expect("draw succeeds");
    let buffer = terminal.backend().buffer().clone();
    app.processes.cancel(id);

    // Everything is asserted relative to the separator, so the search never
    // strays into the dashboard around the pane.
    let cells = buffer.content();
    let separator = cells
        .iter()
        .position(|cell| cell.symbol() == "\u{e0b0}")
        .unwrap_or_else(|| panic!("the separator is on screen:\n{}", terminal.backend()));

    // It survived as a single cell carrying the whole codepoint --- not two
    // cells, and not a replacement character.
    assert!(!buffer.content().iter().any(|c| c.symbol() == "\u{fffd}"));

    // Its colours are the shell's indexed pair, not the app palette: the
    // separator is drawn foreground-31 on background-236, the inversion of
    // the segment before it that makes a powerline prompt look like one.
    assert_eq!(cells[separator].fg, Color::Indexed(31));
    assert_eq!(cells[separator].bg, Color::Indexed(236));

    // The ` dev ` segment right before it keeps its own pair.
    assert_eq!(cells[separator - 4].symbol(), "d");
    assert_eq!(cells[separator - 4].fg, Color::Indexed(236));
    assert_eq!(cells[separator - 4].bg, Color::Indexed(31));

    // And the right-hand segment sits where the `CSI 40 C` put it: the
    // cursor left the separator at +1, jumped 40 columns, and printed a
    // leading space, so the clock's first digit is at +42. The old console
    // could not move the cursor past the end of the line at all.
    assert_eq!(
        cells[separator + 42].symbol(),
        "1",
        "the clock landed 40 columns past the separator:\n{}",
        terminal.backend()
    );
    assert_eq!(cells[separator + 42].fg, Color::Indexed(66));
}

/// A shell that asks where its cursor is must get an answer. `vt100` has no
/// way to write back, so `CSI 6 n` (and the device-attribute queries) reach
/// `TerminalCallbacks` and are answered from there --- unanswered, a prompt
/// that measures itself simply hangs.
#[test]
fn the_terminal_answers_the_queries_a_prompt_blocks_on() {
    let mut app = App::new("/nonexistent-project-dir");
    app.set_terminal_tool(
        Command::new("/bin/sh")
            .arg("-c")
            // Ask, read the six bytes of the report (`ESC [ 1 ; 1 R`),
            // and print them as hex: the pty's input discipline no longer
            // echoes (ChipTUI sets it at spawn, the way a terminal owns
            // its own line settings), so the child itself is the
            // observation --- raw would be pointless, the emulator parses
            // a cursor-position report back into invisibility.
            .arg("printf '\\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -t x1; sleep 30"),
    );
    app.show_terminal_tab();
    let id = app.terminal_process.expect("the shell session started");

    let deadline = Instant::now() + Duration::from_secs(20);
    while !app.terminal.screen().contents().contains("1b 5b") && Instant::now() < deadline {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let screen = app.terminal.screen().contents();
    app.processes.cancel(id);

    assert!(
        screen.contains("1b 5b 31 3b 31 52"),
        "the query was answered with a cursor-position report: {screen:?}"
    );
}

/// The tab's shell is a *login* shell: it sources `~/.profile` (`.zprofile`,
/// `.bash_profile` for the shells that name them differently) --- where a
/// login session's exported variables live, and the difference between the
/// tab's environment and a fresh terminal window's. The real path resolves
/// the user's shell itself, so the probe points `SHELL` at `/bin/sh` and
/// `HOME` at a temp directory whose `.profile` prints a marker: a non-login
/// interactive shell never reads it (that is dash's and bash's own rule), so
/// the marker on screen is the login mode speaking.
#[test]
fn the_tab_shell_starts_as_a_login_shell_and_sources_its_login_files() {
    let home = std::env::temp_dir().join(format!("chiptui-login-shell-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("the probe home exists");
    std::fs::write(home.join(".profile"), "printf 'login-file-sourced\n'\n")
        .expect("the login file is written");

    let mut app = App::new("/nonexistent-project-dir");
    app.set_terminal_tool(
        Command::new("sh")
            .as_login_shell()
            .env("SHELL", "/bin/sh")
            .env("HOME", home.to_str().expect("the probe home is utf-8")),
    );
    app.show_terminal_tab();
    let id = app.terminal_process.expect("the shell session started");

    // The shell stays alive at its prompt after sourcing the file, so wait
    // for the marker rather than for an exit.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !app
        .terminal
        .screen()
        .contents()
        .contains("login-file-sourced")
        && Instant::now() < deadline
    {
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    app.processes.cancel(id);
    let _ = std::fs::remove_dir_all(&home);

    let screen = app.terminal.screen().contents();
    assert!(
        screen.contains("login-file-sourced"),
        "the login file was sourced into the tab's shell: {screen:?}"
    );
}
