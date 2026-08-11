//! ChipTUI entry point.

use std::process::ExitCode;

use chiptui::app::{App, TICK_RATE, app_from_cwd};
use chiptui::event::{AppEvent, EventSource};
use chiptui::{Result, terminal, ui};

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
    let mut app = app_from_cwd()?;
    // Detection runs before the terminal is taken over: if it fails, the error
    // is logged into the pane rather than lost behind the alternate screen.
    app.bootstrap();

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
    }
    Ok(())
}
