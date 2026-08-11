//! Process manager against real child processes.
//!
//! Uses the fake executables in `tests/fixtures/bin` by absolute path, so
//! nothing depends on `PATH` or on a board being attached (`AGENTS.md`
//! §Testing).

#![cfg(unix)]

use std::time::{Duration, Instant};

use chiptui::process::{Command, Outcome, ProcessEvent, ProcessId, ProcessManager, Stream};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/bin/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Collects every event for `id` until it finishes, or panics on timeout.
fn run_to_completion(processes: &mut ProcessManager, id: ProcessId) -> Vec<ProcessEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut events = Vec::new();

    while Instant::now() < deadline {
        for event in processes.drain() {
            let finished = matches!(event, ProcessEvent::Finished { .. });
            if event.id() == id {
                events.push(event);
                if finished {
                    return events;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    panic!("process {id} did not finish; collected {events:?}");
}

fn lines(events: &[ProcessEvent], want: Stream) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            ProcessEvent::Line { stream, text, .. } if *stream == want => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn outcome(events: &[ProcessEvent]) -> &Outcome {
    events
        .iter()
        .find_map(|event| match event {
            ProcessEvent::Finished { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("a finished event")
}

#[test]
fn streams_both_pipes_and_reports_the_exit_code() {
    let mut processes = ProcessManager::new();
    let id = processes.spawn(Command::new(fixture("noisy")), Duration::from_secs(10));
    let events = run_to_completion(&mut processes, id);

    assert_eq!(lines(&events, Stream::Stdout), ["line one", "line two"]);
    assert_eq!(lines(&events, Stream::Stderr), ["warning on stderr"]);
    assert_eq!(outcome(&events), &Outcome::Failed { code: Some(3) });
}

#[test]
fn the_first_event_is_started_and_the_last_is_finished() {
    // The browser relies on this: seeing `Finished` means all output arrived.
    let mut processes = ProcessManager::new();
    let id = processes.spawn(Command::new(fixture("noisy")), Duration::from_secs(10));
    let events = run_to_completion(&mut processes, id);

    assert!(matches!(events.first(), Some(ProcessEvent::Started { .. })));
    assert!(matches!(events.last(), Some(ProcessEvent::Finished { .. })));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ProcessEvent::Finished { .. }))
            .count(),
        1
    );
}

#[test]
fn a_missing_executable_is_an_outcome_not_a_panic() {
    let mut processes = ProcessManager::new();
    let id = processes.spawn(
        Command::new(fixture("does-not-exist")),
        Duration::from_secs(5),
    );
    let events = run_to_completion(&mut processes, id);

    assert!(matches!(outcome(&events), Outcome::SpawnFailed(_)));
    assert!(outcome(&events).summary().contains("could not start"));
}

#[test]
fn a_hung_process_is_killed_at_the_timeout() {
    // The real case: a board that stops responding mid-command.
    let mut processes = ProcessManager::new();
    let started = Instant::now();
    let id = processes.spawn(Command::new(fixture("slow")), Duration::from_millis(300));
    let events = run_to_completion(&mut processes, id);

    assert_eq!(outcome(&events), &Outcome::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "kill was not prompt"
    );
    // Output produced before the kill is still delivered.
    assert_eq!(lines(&events, Stream::Stdout), ["starting"]);
}

#[test]
fn cancelling_stops_a_running_process() {
    let mut processes = ProcessManager::new();
    let id = processes.spawn(Command::new(fixture("slow")), Duration::from_secs(60));

    // Wait until it is actually running before cancelling.
    let deadline = Instant::now() + Duration::from_secs(5);
    while processes.running_count() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    processes.cancel(id);

    let events = run_to_completion(&mut processes, id);
    assert_eq!(outcome(&events), &Outcome::Cancelled);
    assert!(
        !processes.is_running(id),
        "finished processes are forgotten"
    );
}

#[test]
fn arguments_are_passed_without_a_shell() {
    let mut processes = ProcessManager::new();
    // If this went through a shell, `;` would split the command and the
    // unknown-argument branch would not be what answers.
    let id = processes.spawn(
        Command::new(fixture("mpremote")).arg("fs; echo pwned"),
        Duration::from_secs(10),
    );
    let events = run_to_completion(&mut processes, id);

    assert!(
        !lines(&events, Stream::Stdout)
            .iter()
            .any(|line| line.contains("pwned"))
    );
    assert!(matches!(outcome(&events), Outcome::Failed { .. }));
}

#[test]
fn several_processes_run_concurrently_and_stay_distinguishable() {
    let mut processes = ProcessManager::new();
    let first = processes.spawn(Command::new(fixture("noisy")), Duration::from_secs(10));
    let second = processes.spawn(
        Command::new(fixture("mpremote")).arg("devs"),
        Duration::from_secs(10),
    );

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while seen.len() < 2 && Instant::now() < deadline {
        for event in processes.drain() {
            if let ProcessEvent::Finished { id, .. } = event {
                seen.push(id);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(seen.len(), 2);
    assert!(seen.contains(&first) && seen.contains(&second));
    assert_ne!(first, second, "ids are unique");
}

#[test]
fn draining_an_idle_manager_yields_nothing() {
    let mut processes = ProcessManager::new();
    assert!(processes.drain().is_empty());
    assert_eq!(processes.running_count(), 0);
}
