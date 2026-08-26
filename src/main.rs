//! ChipTUI entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use chiptui::app::{App, PendingEdit, TICK_RATE};
use chiptui::backend::BackendRegistry;
use chiptui::event::{AppEvent, EventSource};
use chiptui::home::{HomeOutcome, HomeScreen};
use chiptui::settings::ProjectRegistry;
use chiptui::startup::{self, Route};
use chiptui::{Result, editor, terminal, ui};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The terminal has already been restored by the guard or the panic
            // hook, so this reaches the user's shell intact.
            eprintln!("chiptui: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    // Captured first, before `app.bootstrap()` logs anything and before any
    // other thread exists: `time`'s local-offset lookup (`logs::LogStore`
    // module docs) is unsound once the process is multi-threaded.
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);

    let home = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
    let config_dir = chiptui::settings::default_config_dir(&home);
    let cwd = std::env::current_dir()?;
    // Where the session starts is decided before the terminal is taken
    // over: it is a filesystem question, and a failure here should reach
    // the user's shell rather than the alternate screen.
    let mut route = startup::route(
        &cwd,
        &BackendRegistry::with_builtin_backends(),
        &ProjectRegistry::load(&config_dir, &home),
    );

    // Read before the terminal is taken over: mouse capture is decided by
    // the user config, not probed from the terminal, and the flag rides the
    // guard for the whole session (suspend re-applies it after $EDITOR).
    let mouse = chiptui::settings::mouse(&config_dir);
    let mut guard = terminal::init(mouse)?;
    let mut events = EventSource::new(TICK_RATE);

    // Probed before the TUI takes over: ratatui-image's query talks to the
    // terminal directly, which would fight crossterm's raw mode afterwards.
    // Halfblocks render everywhere, so a failed probe degrades the board
    // pictures' fidelity, never blocks startup.
    let image_picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    // The two screens alternate for as long as the user keeps switching
    // projects: the home screen names one, the dashboard runs it, and
    // dropping the dashboard is what cancels its commands and frees the
    // serial port before the next project claims them.
    let outcome = loop {
        match route {
            Route::Home => match home_loop(&mut guard, &mut events, &config_dir, &home) {
                Ok(Some(dir)) => route = Route::Open(dir),
                Ok(None) => break Ok(()),
                Err(err) => break Err(err),
            },
            Route::Open(dir) => {
                match project_loop(&mut guard, &mut events, dir, offset, image_picker.clone()) {
                    Ok(true) => route = Route::Home,
                    Ok(false) => break Ok(()),
                    Err(err) => break Err(err),
                }
            }
        }
    };

    // Restore explicitly so teardown failures are reported; `Drop` still covers
    // the paths where `?` returned early above.
    let restored = guard.restore();
    outcome.and(restored)
}

/// Lists the recorded projects until one is chosen. `None` means the user
/// quit from the list, which ends the session.
fn home_loop(
    guard: &mut terminal::TerminalGuard,
    events: &mut EventSource,
    config_dir: &std::path::Path,
    home: &std::path::Path,
) -> Result<Option<PathBuf>> {
    let mut screen = HomeScreen::new(config_dir, home);
    // No backend is active on the home screen, so an `Auto` choice renders
    // in the Tokyo Night stand-in there; the per-project loop resolves it
    // against the project's own backend.
    let theme = chiptui::app::resolve_theme(config_dir)
        .resolve(None)
        .palette();
    // The same opt-in `[ui] mouse` answer the dashboard runs under; a
    // gesture is only forwarded when the session asked for reporting.
    let mouse = chiptui::settings::mouse(config_dir);
    loop {
        guard
            .terminal()
            .draw(|frame| ui::home::draw(frame, &screen, theme))?;
        match events.next_event()? {
            AppEvent::Key(key) => match screen.handle_key(key) {
                Some(HomeOutcome::Open(dir)) => return Ok(Some(dir)),
                Some(HomeOutcome::Quit) => return Ok(None),
                None => {}
            },
            AppEvent::Mouse(gesture) if mouse => {
                // The area the drawn frame filled (a resize between the
                // draw and the gesture gets its own redraw first).
                let Ok(size) = guard.terminal().size() else {
                    continue;
                };
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                match screen.on_mouse(gesture, area) {
                    Some(HomeOutcome::Open(dir)) => return Ok(Some(dir)),
                    Some(HomeOutcome::Quit) => return Ok(None),
                    None => {}
                }
            }
            // Ticks and resizes only need the redraw above; the home screen
            // has no processes to poll.
            _ => {}
        }
    }
}

/// Runs the dashboard on `dir`. Returns whether the user asked for the home
/// screen (`true`) rather than to quit --- the `App`, and with it every
/// running command, is dropped either way.
fn project_loop(
    guard: &mut terminal::TerminalGuard,
    events: &mut EventSource,
    dir: PathBuf,
    offset: time::UtcOffset,
    image_picker: ratatui_image::picker::Picker,
) -> Result<bool> {
    let mut app = App::new(dir);
    app.logs.set_offset(offset);
    // Decides which half of the shortcuts overlay's hybrid trigger is live:
    // a bare Ctrl press/release where the terminal's Kitty keyboard
    // protocol answered `terminal::init`'s probe, `ctrl+k` alone otherwise.
    app.set_keyboard_enhanced(guard.keyboard_enhanced());
    // Mirror of the guard's mouse-capture flag: the app trusts gestures
    // only for the sessions whose terminal was asked to report them.
    app.set_mouse_enabled(guard.mouse());
    // The board/shield pickers' online enrichment, wired exactly once per
    // project session: the HTTP transport, the re-fetchable disk cache
    // under the app's own directory conventions, and the terminal-probed
    // image protocol. Tests leave all three unset, which keeps the pickers
    // fully offline.
    app.docs.set_fetch(chiptui::board_docs::http_fetch());
    app.docs
        .set_cache_dir(chiptui::settings::default_cache_dir(app.home_dir()));
    app.docs.set_image_picker(image_picker);
    app.bootstrap();
    // Whatever named the backend --- the registry, a `chiptui.toml`, or the
    // evidence --- this is the one place the project is recorded as opened,
    // so the home screen's list and its ordering stay complete.
    app.record_open_project();
    app.maybe_open_project_setup();
    app.maybe_scan_devices();
    app.place_startup_focus();
    // The Zephyr flow's first question: when no config names the
    // installation, ask right away (`SPEC.md` §10's environment) --- the
    // pane alone would leave the answer one keypress away instead of in
    // the user's face.
    app.maybe_open_workspace_picker();

    event_loop(&mut app, guard, events)?;
    Ok(app.take_switch_request())
}

fn event_loop(
    app: &mut App,
    guard: &mut terminal::TerminalGuard,
    events: &mut EventSource,
) -> Result<()> {
    while !app.should_quit() && !app.switch_requested() {
        guard.terminal().draw(|frame| ui::draw(frame, app))?;

        // Output from external commands is collected before blocking, so the
        // worst-case latency for a streamed line is one tick.
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }
        // Board-docs fetches land the same way, on their own channel.
        for event in app.docs.drain() {
            app.handle(AppEvent::Docs(event));
        }

        let event = events.next_event()?;
        let overlay_was_open = app.overlay.is_some();
        app.handle(event);
        if overlay_was_open && app.overlay.is_none() {
            // Belt-and-suspenders: some terminals can mishandle a partial
            // diff that repaints only part of a wide glyph sitting
            // mid-screen, compared to when that same cell is part of a
            // full repaint. Closing a modal exposes exactly that kind of
            // region as a partial diff; forcing a full repaint here
            // sidesteps the class of inconsistency, on top of every
            // decorative glyph in this app now being picked so no
            // terminal can disagree with `ratatui` about its width in
            // the first place.
            guard.terminal().clear()?;
        }

        // A copy gesture (the MAC row's click, or `Enter` on a focused
        // Device Info pane) becomes the terminal's own clipboard escape
        // here, between frames, where stdout is ours.
        if let Some(text) = app.take_clipboard_request() {
            terminal::set_clipboard(&text)?;
        }
        if let Some(pending) = app.take_pending_edit() {
            run_editor(app, guard, pending)?;
        }
        if let Some(command) = app.take_pending_command() {
            run_interactive(app, guard, command)?;
        }
    }
    Ok(())
}

/// Suspends the alternate screen so `$EDITOR` gets the real terminal, runs it
/// on `pending.path` --- from the project folder, addressing the file
/// relative to it (`cd project && $EDITOR src/main.c`), so an editor whose
/// file explorer follows its working directory opens straight on the
/// project's files --- and reports the outcome the way every other external
/// tool does: into the log, never silently. The spawn/exit result is the
/// editor's business, not a reason to tear down ChipTUI, so only a failure to
/// toggle the terminal itself propagates (`guard.suspend`'s `?`).
///
/// A device-sourced edit (`pending.device_target`) is re-uploaded once the
/// editor exits cleanly --- `:cq`'s conventional non-zero exit is treated as
/// "discard", the same signal git and similar tools already rely on, so a
/// deliberately abandoned edit is not pushed back to the board.
fn run_editor(
    app: &mut App,
    guard: &mut terminal::TerminalGuard,
    pending: PendingEdit,
) -> Result<()> {
    let command = editor::resolve();
    let cwd = app.editor_cwd();
    let target = editor::target_from(&pending.path, &cwd);
    let label = format!("{} {}", command.program, target.display());

    let outcome = guard.suspend(|| {
        std::process::Command::new(&command.program)
            .args(&command.args)
            .arg(&target)
            .current_dir(&cwd)
            .status()
    })?;

    let clean_exit = match outcome {
        Ok(status) if status.success() => {
            app.logs.info(format!("{label} closed"));
            true
        }
        Ok(status) => {
            app.logs.warn(format!("{label} exited with {status}"));
            false
        }
        Err(source) => {
            app.logs.error(format!("could not run {label}: {source}"));
            false
        }
    };
    app.reload_local_files();

    if clean_exit && let Some(target) = pending.device_target {
        app.request_device_reupload(pending.path, target);
    }
    Ok(())
}

/// Runs an interactive external command (`west build -t menuconfig`) with
/// the real terminal, the same suspension `$EDITOR` gets: the child *is* a
/// full-screen program, and anything less than handing over the terminal
/// breaks both interfaces at once. Unlike the editor, this runs a command
/// ChipTUI itself composed ([`crate::process::Command`]: program, args,
/// environment, working directory), so its environment and cwd match the
/// piped west commands exactly. The outcome is logged, never fatal to the
/// TUI.
fn run_interactive(
    app: &mut App,
    guard: &mut terminal::TerminalGuard,
    command: chiptui::process::Command,
) -> Result<()> {
    let label = command.to_string();
    let program = command.program().to_string();
    let args: Vec<String> = command.args_slice().to_vec();
    let cwd = command.cwd().cloned();
    let envs: Vec<(String, String)> = command.envs_slice().to_vec();

    let outcome = guard.suspend(|| {
        let mut child = std::process::Command::new(&program);
        child.args(&args);
        if let Some(cwd) = &cwd {
            child.current_dir(cwd);
        }
        for (key, value) in &envs {
            child.env(key, value);
        }
        child.status()
    })?;

    match outcome {
        Ok(status) if status.success() => {
            app.logs.info(format!("{label} closed"));
        }
        Ok(status) => {
            app.logs.warn(format!(
                "{label} exited with {status} (changes may be partial)"
            ));
        }
        Err(source) => {
            app.logs.error(format!("could not run {label}: {source}"));
        }
    }
    Ok(())
}
