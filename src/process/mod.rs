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
use std::io::{BufReader, Read, Write};
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
    Output {
        id: ProcessId,
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
            Self::Started { id, .. }
            | Self::Line { id, .. }
            | Self::Output { id, .. }
            | Self::Finished { id, .. } => *id,
        }
    }
}

/// Handle on a running child.
///
/// Only the supervisor thread touches the [`std::process::Child`], so no lock
/// is needed: cancellation is a flag the supervisor polls.
struct Running {
    cancelled: Arc<AtomicBool>,
    stdin_writer: Option<Box<dyn std::io::Write + Send>>,
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
                stdin_writer: None,
            },
        );

        if let Some(stdout) = stdout {
            let tx = self.tx.clone();
            thread::spawn(move || pump(stdout, Stream::Stdout, id, &tx));
        }
        if let Some(stderr) = stderr {
            let tx = self.tx.clone();
            thread::spawn(move || pump(stderr, Stream::Stderr, id, &tx));
        }

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
            // We don't join readers anymore to simplify the code and avoid the LLVM crash.
            // When child dies, pipes close, and pump threads naturally exit.
            // If they are late, it's fine.

            let _ = tx.send(ProcessEvent::Finished {
                id,
                outcome,
                duration: started.elapsed(),
            });
        });

        id
    }

    /// Starts `command` inside a pseudo-terminal (PTY) and returns its ID.
    pub fn spawn_pty(&mut self, command: Command, timeout: Duration) -> Result<ProcessId, String> {
        let id = ProcessId(self.next_id);
        self.next_id += 1;

        let label = command.to_string();
        let _ = self.tx.send(ProcessEvent::Started {
            id,
            label: label.clone(),
        });

        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;

        let mut cmd = portable_pty::CommandBuilder::new(command.program());
        cmd.args(command.args_slice());

        let started = Instant::now();
        let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

        let cancelled = Arc::new(AtomicBool::new(false));

        self.running.insert(
            id,
            Running {
                cancelled: Arc::clone(&cancelled),
                stdin_writer: Some(writer),
            },
        );

        let tx = self.tx.clone();
        let reader_thread = thread::spawn(move || pump_pty(reader, id, &tx));

        let tx = self.tx.clone();
        thread::spawn(move || {
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
                    Ok(Some(status)) => {
                        let outcome = kill_reason.unwrap_or(if status.success() {
                            Outcome::Success
                        } else {
                            Outcome::Failed { code: None }
                        });
                        if !outcome.was_killed() {
                            let _ = reader_thread.join();
                        }
                        let _ = tx.send(ProcessEvent::Finished {
                            id,
                            outcome,
                            duration: started.elapsed(),
                        });
                        break;
                    }
                    Ok(None) => thread::sleep(POLL_INTERVAL),
                    Err(_) => break,
                }
            }
        });

        Ok(id)
    }

    pub fn write_stdin(&mut self, id: ProcessId, data: &[u8]) {
        if let Some(writer) = self
            .running
            .get_mut(&id)
            .and_then(|r| r.stdin_writer.as_mut())
        {
            let _ = writer.write_all(data);
            let _ = writer.flush();
        }
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

/// Forwards one stream line by line until EOF, splitting on `\r` as well as
/// `\n`.
///
/// A plain `\n`-only split leaves a `\r`-driven progress bar (esptool's
/// `write_flash`, for one: it prints `"Writing at 0x...(NN %)"` with `end='\r'`
/// and only emits a real `\n` once, at completion) invisible until the whole
/// command finishes --- `read_until(b'\n', ..)` would block, buffering every
/// update into one giant line, so the UI never sees progress and appears
/// frozen. Treating `\r` as a line boundary too makes each update its own
/// [`ProcessEvent::Line`], while a `\r\n` pair still collapses to one line
/// break rather than an extra empty line.
///
/// Lines are decoded lossily: a filename with invalid UTF-8 should show up as
/// replacement characters, not abort the listing.
fn pump<R: Read>(source: R, stream: Stream, id: ProcessId, tx: &Sender<ProcessEvent>) {
    use std::io::BufRead;

    let mut reader = BufReader::new(source);
    let mut buffer: Vec<u8> = Vec::new();

    loop {
        let available = match reader.fill_buf() {
            Ok([]) => break, // EOF
            Ok(bytes) => bytes,
            Err(_) => break,
        };

        let Some(pos) = available.iter().position(|&b| b == b'\n' || b == b'\r') else {
            // No line boundary in what's buffered yet: accumulate and read more.
            buffer.extend_from_slice(available);
            let len = available.len();
            reader.consume(len);
            continue;
        };

        let delimiter = available[pos];
        buffer.extend_from_slice(&available[..pos]);
        reader.consume(pos + 1);

        if delimiter == b'\r'
            && let Ok(next) = reader.fill_buf()
            && next.first() == Some(&b'\n')
        {
            reader.consume(1);
        }

        let text = String::from_utf8_lossy(&buffer).into_owned();
        buffer.clear();
        if tx.send(ProcessEvent::Line { id, stream, text }).is_err() {
            break;
        }
    }

    // A final partial line with no trailing newline (common right before a
    // process exits) is still worth delivering.
    if !buffer.is_empty() {
        let text = String::from_utf8_lossy(&buffer).into_owned();
        let _ = tx.send(ProcessEvent::Line { id, stream, text });
    }
}

/// Forwards unbuffered stream output for interactive pseudo-terminals.
fn pump_pty<R: Read>(mut source: R, id: ProcessId, tx: &Sender<ProcessEvent>) {
    let mut buffer = [0u8; 1024];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                if tx.send(ProcessEvent::Output { id, text }).is_err() {
                    break;
                }
            }
            Err(_) => break,
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
