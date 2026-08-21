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
fn carriage_return_progress_updates_stream_as_separate_lines() {
    // Regression: esptool's write_flash progress uses `\r` with no `\n`
    // until the very end. A pump that only splits on `\n` buffers every
    // update into one giant line, so the UI never sees progress until the
    // command is already finished. Each `\r`-terminated update must arrive
    // as its own event, and a trailing `\r\n` must not produce a stray
    // empty line.
    let mut processes = ProcessManager::new();
    let id = processes.spawn(Command::new(fixture("progress")), Duration::from_secs(10));
    let events = run_to_completion(&mut processes, id);

    assert_eq!(
        lines(&events, Stream::Stdout),
        [
            "Writing at 0x00001000... (10 %)",
            "Writing at 0x00001000... (50 %)",
            "Writing at 0x00001000... (100 %)",
            "Wrote 16384 bytes",
        ]
    );
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
fn a_pty_child_runs_in_the_commands_working_directory() {
    // The Terminal tab's shell reaches its project directory through this
    // path: `spawn_pty` must honour `Command::current_dir`, not just the
    // program and arguments.
    let mut processes = ProcessManager::new();
    let dir = std::env::temp_dir()
        .join(format!("chiptui-pty-cwd-{}", std::process::id()))
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().expect("the temp dir exists");

    let id = processes
        .spawn_pty(
            Command::new("/bin/sh")
                .arg("-c")
                .arg("pwd")
                .current_dir(&dir),
            Duration::from_secs(10),
        )
        .expect("the pty opened");
    let events = run_to_completion(&mut processes, id);

    assert_eq!(outcome(&events), &Outcome::Success);
    let output: String = events
        .iter()
        .filter_map(|event| match event {
            ProcessEvent::Output { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let expected = dir.display().to_string();
    assert!(
        output.trim().contains(&expected),
        "the pty child must land in the given cwd; pwd printed {output:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
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

#[test]
fn output_written_just_before_exit_arrives_before_finished() {
    // Regression: the supervisor used to report `Finished` without waiting
    // for the reader threads, so lines still in flight under load were lost
    // on consumers that stop tracking a process once it finishes --- an
    // `mpremote devs` reply occasionally parsed as "no devices found".
    let mut processes = ProcessManager::new();
    let id = processes.spawn(Command::new(fixture("bursty")), Duration::from_secs(10));
    let events = run_to_completion(&mut processes, id);

    let stdout = lines(&events, Stream::Stdout);
    assert_eq!(stdout.len(), 500, "every line of the burst survives");
    assert_eq!(stdout.first().map(String::as_str), Some("burst 0"));
    assert_eq!(stdout.last().map(String::as_str), Some("burst 499"));
    assert!(matches!(events.last(), Some(ProcessEvent::Finished { .. })));
}

/// A PTY child's exit code is as real as a piped one's. The PTY path used
/// to collapse every failure to `Failed { code: None }`, which
/// `Outcome::summary` reports as "terminated by signal" --- so a shell that
/// simply exited 3 was described as having been killed.
#[test]
fn a_pty_child_reports_its_exit_code() {
    let mut processes = ProcessManager::new();
    let id = processes
        .spawn_pty(
            Command::new("/bin/sh").arg("-c").arg("exit 3"),
            Duration::from_secs(10),
        )
        .expect("the pty opened");
    let events = run_to_completion(&mut processes, id);
    assert_eq!(outcome(&events), &Outcome::Failed { code: Some(3) });
}

/// A raw PTY reports bytes, not decoded text --- and that is what keeps a
/// multi-byte character split across the reader's 1 KiB buffer intact.
/// Decoding each chunk on its own replaces both halves with U+FFFD, which
/// is exactly what happened to every powerline glyph in a shell prompt.
#[test]
fn a_raw_pty_hands_back_bytes_a_glyph_can_survive() {
    let mut processes = ProcessManager::new();
    let id = processes
        .spawn_pty_raw(
            // 1023 padding bytes, so the three-byte U+E0B0 straddles the
            // 1024-byte boundary.
            Command::new("/bin/sh")
                .arg("-c")
                .arg("printf 'x%.0s' $(seq 1023); printf '\\356\\202\\260'"),
            Duration::from_secs(10),
            24,
            80,
        )
        .expect("the pty opened");
    let events = run_to_completion(&mut processes, id);

    let raw: Vec<u8> = events
        .iter()
        .filter_map(|event| match event {
            ProcessEvent::Bytes { data, .. } => Some(data.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    let text = String::from_utf8(raw).expect("raw bytes reassemble into valid UTF-8");
    assert!(
        text.contains('\u{e0b0}'),
        "the separator crossed the read boundary whole"
    );
    assert!(!text.contains('\u{fffd}'), "nothing was replaced");
}

/// Resizing reaches the child as a real window change. Without the PTY's
/// master kept alive there is nothing to resize through, and a shell keeps
/// laying out its prompt against 80 columns forever.
#[test]
fn a_pty_child_is_told_when_its_window_changes() {
    let mut processes = ProcessManager::new();
    let id = processes
        .spawn_pty_raw(
            Command::new("/bin/sh")
                .arg("-c")
                .arg("trap 'stty size' WINCH; stty size; while :; do sleep 0.05; done"),
            Duration::from_secs(20),
            10,
            40,
        )
        .expect("the pty opened");

    let collect = |processes: &mut ProcessManager, needle: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut seen = String::new();
        while std::time::Instant::now() < deadline {
            for event in processes.drain() {
                if let ProcessEvent::Bytes { data, .. } = event {
                    seen.push_str(&String::from_utf8_lossy(&data));
                }
            }
            if seen.contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    };

    assert!(
        collect(&mut processes, "10 40"),
        "the child opened at the size it was given"
    );
    processes.resize_pty(id, 24, 100);
    assert!(
        collect(&mut processes, "24 100"),
        "the resize reached the child as a SIGWINCH"
    );
    processes.cancel(id);
}
