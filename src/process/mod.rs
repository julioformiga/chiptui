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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// How long a natural exit will wait for late reader threads before
/// reporting `Finished` anyway. Bounded so a grandchild still holding the
/// pipes cannot stall the report indefinitely; generous enough that a
/// reader merely waiting for scheduling under load still makes it.
const READ_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Waits until every reader thread has finished, or `READ_DRAIN_TIMEOUT`
/// passes. Called only on natural exits: a killed process's grandchildren
/// can keep the pipes open forever, which is exactly the hang the timeout
/// exists to escape.
fn wait_for_readers(readers: &Arc<AtomicUsize>) {
    let deadline = Instant::now() + READ_DRAIN_TIMEOUT;
    while readers.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
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
    /// Raw PTY output, undecoded. A terminal emulator has to see the bytes:
    /// [`Self::Output`]'s per-chunk `from_utf8_lossy` turns any multi-byte
    /// character straddling a read boundary into U+FFFD, which is exactly
    /// what a powerline glyph is. Emitted instead of [`Self::Output`] by a
    /// PTY spawned raw (see [`ProcessManager::spawn_pty_raw`]).
    Bytes {
        id: ProcessId,
        data: Vec<u8>,
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
            | Self::Bytes { id, .. }
            | Self::Finished { id, .. } => *id,
        }
    }
}

/// The size a PTY opens at when nobody asks for one: the conventional
/// terminal, which is what a line-oriented child (mpremote's REPL, the
/// script probe) sees today and has no opinion about.
const DEFAULT_PTY_SIZE: (u16, u16) = (24, 80);

/// Handle on a running child.
///
/// Only the supervisor thread touches the [`std::process::Child`], so no lock
/// is needed: cancellation is a flag the supervisor polls.
struct Running {
    cancelled: Arc<AtomicBool>,
    stdin_writer: Option<Box<dyn std::io::Write + Send>>,
    /// The PTY's master end, kept alive so the child can be told its window
    /// changed size ([`ProcessManager::resize_pty`]). Dropping it right
    /// after `spawn_pty` --- which is what used to happen --- leaves the
    /// cloned reader and taken writer working but puts `MasterPty::resize`
    /// permanently out of reach, so the child believes in 80 columns
    /// forever. `None` for a piped child, which has no window.
    master: Option<Box<dyn portable_pty::MasterPty>>,
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
        let readers = Arc::new(AtomicUsize::new(0));

        self.running.insert(
            id,
            Running {
                cancelled: Arc::clone(&cancelled),
                stdin_writer: None,
                master: None,
            },
        );

        if let Some(stdout) = stdout {
            readers.fetch_add(1, Ordering::Relaxed);
            let tx = self.tx.clone();
            let readers = Arc::clone(&readers);
            thread::spawn(move || {
                pump(stdout, Stream::Stdout, id, &tx, &readers);
            });
        }
        if let Some(stderr) = stderr {
            readers.fetch_add(1, Ordering::Relaxed);
            let tx = self.tx.clone();
            let readers = Arc::clone(&readers);
            thread::spawn(move || {
                pump(stderr, Stream::Stderr, id, &tx, &readers);
            });
        }

        let tx = self.tx.clone();
        thread::spawn(move || {
            let outcome = supervise(&mut child, &cancelled, started, timeout);

            // Draining the pipes before reporting completion is what lets the
            // UI treat `Finished` as "all output has arrived". Joining the
            // reader threads was removed once (deadlock when a grandchild
            // holds the pipes, plus toolchain trouble), so this waits on a
            // counter instead --- bounded, and only for natural exits, where
            // the pipes are about to close anyway and the readers are merely
            // waiting to be scheduled. Late lines from a *killed* process are
            // still dropped on purpose; consumers stop tracking it once it
            // has finished.
            if !outcome.was_killed() {
                wait_for_readers(&readers);
            }
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

    /// Starts `command` inside a pseudo-terminal (PTY) and returns its ID,
    /// at the conventional 24x80 with output decoded into
    /// [`ProcessEvent::Output`] --- what a line-oriented consumer
    /// ([`crate::console::LineConsole`]) wants.
    pub fn spawn_pty(&mut self, command: Command, timeout: Duration) -> Result<ProcessId, String> {
        self.spawn_pty_with(command, timeout, DEFAULT_PTY_SIZE, false)
    }

    /// The same, but sized to a real pane and reporting *raw* bytes
    /// ([`ProcessEvent::Bytes`]) --- what a terminal emulator wants, since
    /// decoding per read chunk mangles any character split across one.
    pub fn spawn_pty_raw(
        &mut self,
        command: Command,
        timeout: Duration,
        rows: u16,
        cols: u16,
    ) -> Result<ProcessId, String> {
        self.spawn_pty_with(command, timeout, (rows.max(1), cols.max(1)), true)
    }

    fn spawn_pty_with(
        &mut self,
        command: Command,
        timeout: Duration,
        (rows, cols): (u16, u16),
        raw: bool,
    ) -> Result<ProcessId, String> {
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
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;

        let mut cmd = portable_pty::CommandBuilder::new(command.program());
        cmd.args(command.args_slice());
        // The structured command's whole setting: a working directory and
        // environment overrides belong to the child as much as the program
        // does (the Terminal tab's shell asks for the project root this
        // way). portable-pty inherits the parent environment for anything
        // not named here, `TERM` included --- what a terminal child wants.
        if let Some(cwd) = command.cwd() {
            cmd.cwd(cwd);
        }
        for (key, value) in command.envs_slice() {
            cmd.env(key, value);
        }

        let started = Instant::now();
        let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

        let cancelled = Arc::new(AtomicBool::new(false));
        let readers = Arc::new(AtomicUsize::new(1));

        self.running.insert(
            id,
            Running {
                cancelled: Arc::clone(&cancelled),
                stdin_writer: Some(writer),
                master: Some(pair.master),
            },
        );

        let tx = self.tx.clone();
        let reader_thread = thread::spawn(move || pump_pty(reader, id, &tx, &readers, raw));

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
                            // The exit code is as real on a PTY child as on a
                            // piped one (`supervise` keeps it); reporting
                            // `None` here read as "terminated by signal".
                            Outcome::Failed {
                                code: i32::try_from(status.exit_code()).ok(),
                            }
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
                    Err(source) => {
                        // Matches `supervise`'s own handling of the same
                        // error: without a `Finished` event the entry in
                        // `running` is never removed, so `is_running(id)`
                        // stays true forever and the panel that started this
                        // PTY is stuck reporting "busy".
                        let _ = tx.send(ProcessEvent::Finished {
                            id,
                            outcome: Outcome::SpawnFailed(source.to_string()),
                            duration: started.elapsed(),
                        });
                        break;
                    }
                }
            }
        });

        Ok(id)
    }

    /// Tells a PTY child its window changed size (the SIGWINCH a shell and
    /// every full-screen program redraw on). A no-op for a piped child or
    /// one that has already finished.
    pub fn resize_pty(&mut self, id: ProcessId, rows: u16, cols: u16) {
        if let Some(master) = self.running.get(&id).and_then(|r| r.master.as_ref()) {
            let _ = master.resize(portable_pty::PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            });
        }
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
///
/// `readers` is decremented on the way out, whatever happened: it is what
/// the supervisor waits on before reporting a natural exit.
fn pump<R: Read>(
    source: R,
    stream: Stream,
    id: ProcessId,
    tx: &Sender<ProcessEvent>,
    readers: &AtomicUsize,
) {
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
    readers.fetch_sub(1, Ordering::Relaxed);
}

/// Forwards unbuffered stream output for interactive pseudo-terminals.
fn pump_pty<R: Read>(
    mut source: R,
    id: ProcessId,
    tx: &Sender<ProcessEvent>,
    readers: &AtomicUsize,
    raw: bool,
) {
    let mut buffer = [0u8; 1024];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => break, // EOF
            Ok(n) => {
                // Raw consumers parse the bytes themselves; decoding here
                // would replace any character straddling the 1 KiB boundary
                // with U+FFFD before they ever saw it.
                let event = if raw {
                    ProcessEvent::Bytes {
                        id,
                        data: buffer[..n].to_vec(),
                    }
                } else {
                    ProcessEvent::Output {
                        id,
                        text: String::from_utf8_lossy(&buffer[..n]).into_owned(),
                    }
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    readers.fetch_sub(1, Ordering::Relaxed);
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
