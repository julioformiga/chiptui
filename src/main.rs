//! ChipTUI entry point.

use std::process::ExitCode;

use chiptui::app::{App, PendingEdit, TICK_RATE, app_from_cwd};
use chiptui::event::{AppEvent, EventSource};
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

    let mut app = app_from_cwd()?;
    app.logs.set_offset(offset);
    // Detection runs before the terminal is taken over: if it fails, the error
    // is logged into the pane rather than lost behind the alternate screen.
    app.bootstrap();
    app.maybe_open_project_setup();
    app.maybe_scan_devices();
    // The Zephyr flow's first question: when no config names the
    // installation, ask right away (`SPEC.md` §10's environment) --- the
    // pane alone would leave the answer one keypress away instead of in
    // the user's face.
    app.maybe_open_workspace_picker();

    let mut guard = terminal::init()?;
    let mut events = EventSource::new(TICK_RATE);

    let outcome = event_loop(&mut app, &mut guard, &mut events);

    // Restore explicitly so teardown failures are reported; `Drop` still covers
    // the paths where `?` returned early above.
    let restored = guard.restore();
    outcome.and(restored)
}

fn event_loop(
    app: &mut App,
    guard: &mut terminal::TerminalGuard,
    events: &mut EventSource,
) -> Result<()> {
    while !app.should_quit() {
        guard.terminal().draw(|frame| ui::draw(frame, app))?;

        // Output from external commands is collected before blocking, so the
        // worst-case latency for a streamed line is one tick.
        for event in app.processes.drain() {
            app.handle(AppEvent::Process(event));
        }

        let event = events.next_event()?;
        app.handle(event);

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
/// on `pending.path`, and reports the outcome the way every other external
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
    let label = format!("{} {}", command.program, pending.path.display());

    let outcome = guard.suspend(|| {
        std::process::Command::new(&command.program)
            .args(&command.args)
            .arg(&pending.path)
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
