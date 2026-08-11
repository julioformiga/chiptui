//! Process execution off the UI thread.
//!
//! `AGENTS.md` §5: external commands must never block the event loop, and must
//! support streaming, exit status, cancellation, timeouts and cleanup.
//!
//! Each process gets one supervisor thread and two reader threads. For a
//! process that exits on its own, the supervisor joins the readers before
//! reporting [`ProcessEvent::Finished`], so a consumer that has seen `Finished`
//! has already seen every line that process produced. A process we *killed* is
//! reported immediately instead --- see [`ProcessManager::spawn`] for why.
//!
//! Everything reaches the UI through one channel, drained once per frame by
//! [`ProcessManager::drain`] --- no locks and no async runtime (`AGENTS.md`:
//! "avoid premature async").

mod command;

use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

pub use command::Command;

/// How often the supervisor checks for exit, cancellation and timeout.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessId(u64);

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// How a process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// Ran to completion with a non-zero status. `code` is `None` when the
    /// process was terminated by a signal.
    Failed {
        code: Option<i32>,
    },
    /// The executable could not be started --- usually "not found on PATH".
    SpawnFailed(String),
    /// Killed after exceeding its timeout.
    TimedOut,
    /// Killed at the user's request.
    Cancelled,
}

impl Outcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Whether the process was terminated by us rather than exiting on its own.
    pub fn was_killed(&self) -> bool {
        matches!(self, Self::TimedOut | Self::Cancelled)
    }

    /// Short description for the log pane.
    pub fn summary(&self) -> String {
        match self {
            Self::Success => "ok".to_string(),
            Self::Failed { code: Some(code) } => format!("exit code {code}"),
            Self::Failed { code: None } => "terminated by signal".to_string(),
            Self::SpawnFailed(reason) => format!("could not start: {reason}"),
            Self::TimedOut => "timed out".to_string(),
            Self::Cancelled => "cancelled".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEvent {
    Started {
        id: ProcessId,
        /// The command as it was run, for the log and for diagnostics.
        label: String,
    },
    Line {
        id: ProcessId,
        stream: Stream,
        text: String,
    },
    Finished {
        id: ProcessId,
        outcome: Outcome,
        duration: Duration,
    },
}

impl ProcessEvent {
    pub fn id(&self) -> ProcessId {
        match self {
            Self::Started { id, .. } | Self::Line { id, .. } | Self::Finished { id, .. } => *id,
        }
    }
}

/// Handle on a running child.
///
/// Only the supervisor thread touches the [`std::process::Child`], so no lock
/// is needed: cancellation is a flag the supervisor polls.
struct Running {
    cancelled: Arc<AtomicBool>,
}

pub struct ProcessManager {
    tx: Sender<ProcessEvent>,
    rx: Receiver<ProcessEvent>,
    running: HashMap<ProcessId, Running>,
    next_id: u64,
}

impl ProcessManager {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            running: HashMap::new(),
            next_id: 1,
        }
    }

    /// Starts `command`, killing it if it outlives `timeout`.
    ///
    /// Returns immediately. A failure to spawn is reported through the event
    /// stream like any other outcome, so callers have a single code path.
    pub fn spawn(&mut self, command: Command, timeout: Duration) -> ProcessId {
        let id = ProcessId(self.next_id);
        self.next_id += 1;

        let label = command.to_string();
        let _ = self.tx.send(ProcessEvent::Started {
            id,
            label: label.clone(),
        });

        let started = Instant::now();
        let mut child = match command.to_std().spawn() {
            Ok(child) => child,
            Err(source) => {
                let _ = self.tx.send(ProcessEvent::Finished {
                    id,
                    outcome: Outcome::SpawnFailed(source.to_string()),
                    duration: started.elapsed(),
                });
                return id;
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let cancelled = Arc::new(AtomicBool::new(false));

        self.running.insert(
            id,
            Running {
                cancelled: Arc::clone(&cancelled),
            },
        );

        let readers: Vec<_> = [
            (stdout.map(Readable::Out), Stream::Stdout),
            (stderr.map(Readable::Err), Stream::Stderr),
        ]
        .into_iter()
        .filter_map(|(source, stream)| source.map(|source| (source, stream)))
        .map(|(source, stream)| {
            let tx = self.tx.clone();
            thread::spawn(move || pump(source, stream, id, &tx))
        })
        .collect();

        let tx = self.tx.clone();
        thread::spawn(move || {
            let outcome = supervise(&mut child, &cancelled, started, timeout);

            // Draining the pipes before reporting completion is what lets the
            // UI treat `Finished` as "all output has arrived".
            //
            // Not so for a process we killed: `kill` reaches only the direct
            // child, and any grandchild it left behind still holds the write
            // end of these pipes, so the readers would block until *that*
            // exits --- which is precisely the hang the timeout exists to
            // escape. Report immediately instead and let the readers end on
            // their own; late lines are dropped, because consumers stop
            // tracking a process once it has finished.
            if !outcome.was_killed() {
                for reader in readers {
                    let _ = reader.join();
                }
            }

            let _ = tx.send(ProcessEvent::Finished {
                id,
                outcome,
                duration: started.elapsed(),
            });
        });

        id
    }

    /// Asks a running process to stop. Takes effect within [`POLL_INTERVAL`].
    pub fn cancel(&mut self, id: ProcessId) {
        if let Some(running) = self.running.get(&id) {
            running.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub fn cancel_all(&mut self) {
        let ids: Vec<ProcessId> = self.running.keys().copied().collect();
        for id in ids {
            self.cancel(id);
        }
    }

    /// Collects everything that arrived since the last call. Never blocks.
    pub fn drain(&mut self) -> Vec<ProcessEvent> {
        let mut events = Vec::new();
        // `self` owns a sender, so `try_recv` can only fail with `Empty`;
        // either way an error just means "nothing more right now".
        while let Ok(event) = self.rx.try_recv() {
            if let ProcessEvent::Finished { id, .. } = &event {
                self.running.remove(id);
            }
            events.push(event);
        }
        events
    }

    pub fn is_running(&self, id: ProcessId) -> bool {
        self.running.contains_key(&id)
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        // Never leave a child holding a serial port after the TUI exits.
        self.cancel_all();
    }
}

/// The two pipe types, unified so both can be pumped by one function.
enum Readable {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Read for Readable {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Out(inner) => inner.read(buf),
            Self::Err(inner) => inner.read(buf),
        }
    }
}

/// Forwards one stream line by line until EOF.
///
/// Lines are decoded lossily: a filename with invalid UTF-8 should show up as
/// replacement characters, not abort the listing.
fn pump(source: Readable, stream: Stream, id: ProcessId, tx: &Sender<ProcessEvent>) {
    use std::io::BufRead;

    let mut reader = BufReader::new(source);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                    buffer.pop();
                }
                let text = String::from_utf8_lossy(&buffer).into_owned();
                if tx.send(ProcessEvent::Line { id, stream, text }).is_err() {
                    break;
                }
            }
        }
    }
}

/// Waits for the child, killing it on cancellation or timeout.
fn supervise(
    child: &mut std::process::Child,
    cancelled: &AtomicBool,
    started: Instant,
    timeout: Duration,
) -> Outcome {
    let mut kill_reason: Option<Outcome> = None;

    loop {
        if kill_reason.is_none() {
            if cancelled.load(Ordering::Relaxed) {
                kill_reason = Some(Outcome::Cancelled);
            } else if started.elapsed() >= timeout {
                kill_reason = Some(Outcome::TimedOut);
            }
            if kill_reason.is_some() {
                let _ = child.kill();
            }
        }

        match child.try_wait() {
            // The kill reason wins: a process killed for timing out did exit
            // with a status, but reporting it would hide why it died.
            Ok(Some(status)) => {
                return kill_reason.unwrap_or(if status.success() {
                    Outcome::Success
                } else {
                    Outcome::Failed {
                        code: status.code(),
                    }
                });
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(source) => return Outcome::SpawnFailed(source.to_string()),
        }
    }
}
