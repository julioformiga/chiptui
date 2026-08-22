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
//! Cancellation kills the child's whole process group, not just the child:
//! `west`/`mpremote` spawn helpers whose survival past the parent's death
//! would be a "cancelled but still running" command. It asks before it
//! insists (`SIGTERM`, then `SIGKILL` a grace later), so a tool with state
//! on disk or on a board gets to close it. See [`signal_group`] and
//! [`supervise`]; teardown runs the same shape synchronously, from
//! [`ProcessManager::shutdown`], because at exit no supervisor thread is
//! guaranteed another turn.
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

/// How long a cancelled or timed-out child gets to wind up on `SIGTERM`
/// before `SIGKILL` (see [`supervise`]). Long enough for a tool to close
/// what it has open, short enough that `Stop` still feels immediate --- the
/// pane says "stopping" for this long at most.
const KILL_GRACE: Duration = Duration::from_millis(250);

/// The same, at teardown ([`ProcessManager::shutdown`]) --- shorter,
/// because it is spent between the user's last keystroke and the terminal
/// coming back, and it is a *total* budget rather than per child.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(200);

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
    /// The child's pid, which is also its process-group id (every child is
    /// spawned as its own group leader --- `Command::to_std`'s
    /// `process_group(0)`, and portable-pty's `setsid` for a PTY child).
    /// Kept here, beside the `Child` the supervisor thread owns, because
    /// [`ProcessManager::shutdown`] has to reach the group *without* that
    /// thread: at exit there is no guarantee it will ever be scheduled
    /// again. `None` when the platform did not report one.
    pid: Option<u32>,
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
                pid: Some(child.id()),
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
            // Not so for a process we killed: even with the group kill
            // closing the pipes, a grandchild that escaped its group (a
            // daemon calling setsid, say) still holds the write end, so the
            // readers would block until *that* exits --- which is precisely
            // the hang the timeout exists to escape. Report immediately
            // instead and let the readers end on their own; late lines are
            // dropped, because consumers stop tracking a process once it
            // has finished.
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

        let mut cmd = pty_command(&command);
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
                pid: child.process_id(),
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

    /// Kills every running child *before this process returns* --- the
    /// teardown path, called from `Drop`.
    ///
    /// [`Self::cancel`] cannot do this job: it stores a flag that a
    /// supervisor thread polls every [`POLL_INTERVAL`], and at exit there is
    /// no guarantee that thread is ever scheduled again --- `main` returns,
    /// the process ends, and the threads die with it. Quitting ChipTUI with
    /// a build running left `west` and its `ninja` behind exactly that way,
    /// and a `mpremote` outliving the TUI keeps the serial port. Since
    /// children are no longer in ChipTUI's process group
    /// (`Command::to_std`), the terminal's own `SIGHUP` no longer cleans up
    /// for us either.
    ///
    /// So this signals the groups itself, from the pids in `running`, with
    /// the same ask-then-insist shape as [`supervise`] --- bounded by
    /// [`SHUTDOWN_GRACE`] in total, since nothing here is worth making the
    /// user wait for. The flags are still raised first, so a supervisor that
    /// *does* get scheduled reports `Cancelled` rather than a signal death.
    ///
    /// What this still cannot cover is everything that never reaches `Drop`:
    /// a ChipTUI killed outright (`SIGKILL`, or the `SIGHUP` of a closing
    /// terminal window), and a panic in a release build, which aborts
    /// (`Cargo.toml`'s `panic = "abort"`) without unwinding. Both would need
    /// process-global state the signal handler and the panic hook could
    /// reach --- `terminal::install_panic_hook` restores the terminal from
    /// exactly such a place --- which is its own change, not this one.
    pub fn shutdown(&mut self) {
        self.cancel_all();
        let pids: Vec<u32> = self.running.values().filter_map(|r| r.pid).collect();
        if pids.is_empty() {
            return;
        }
        for &pid in &pids {
            signal_group(pid, Signal::Term);
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline && pids.iter().copied().any(group_alive) {
            thread::sleep(Duration::from_millis(5));
        }
        for &pid in &pids {
            if group_alive(pid) {
                signal_group(pid, Signal::Kill);
            }
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

/// The PTY child of a structured command. A login-shell command maps to
/// portable-pty's *default program*: empty argv, which makes the crate
/// resolve the user's shell itself (`$SHELL`, then the passwd entry) and
/// exec it with `argv[0]` prefixed by `-` --- the login convention, and the
/// one supported way to reach it. Any other command is its program plus
/// arguments. Both kinds start from the parent's full environment; the
/// difference is only which shell startup files the child then sources,
/// and a login shell is what gives the Terminal tab the environment a
/// fresh terminal window has (`.zprofile`/`.profile` exports included).
fn pty_command(command: &Command) -> portable_pty::CommandBuilder {
    if command.is_login_shell() {
        portable_pty::CommandBuilder::new_default_prog()
    } else {
        let mut builder = portable_pty::CommandBuilder::new(command.program());
        builder.args(command.args_slice());
        builder
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        // Never leave a child holding a serial port after the TUI exits ---
        // and `cancel_all` alone never kept that promise, see `shutdown`.
        self.shutdown();
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
///
/// The kill is two phases, not one. [`Signal::Term`] goes out first, so a
/// tool that cleans up after itself gets to: `esptool` mid-`write-flash` and
/// `ninja` mid-link both have state on disk (and, for esptool, on a board)
/// that an unblockable `SIGKILL` leaves half-written. Only a child still
/// alive [`KILL_GRACE`] later gets [`Signal::Kill`] --- which is also the
/// direct [`std::process::Child::kill`], the fallback for a platform without
/// groups and for a child that somehow left its own.
fn supervise(
    child: &mut std::process::Child,
    cancelled: &AtomicBool,
    started: Instant,
    timeout: Duration,
) -> Outcome {
    let mut kill_reason: Option<Outcome> = None;
    let mut asked_at: Option<Instant> = None;
    let pid = child.id();

    loop {
        if kill_reason.is_none() {
            if cancelled.load(Ordering::Relaxed) {
                kill_reason = Some(Outcome::Cancelled);
            } else if started.elapsed() >= timeout {
                kill_reason = Some(Outcome::TimedOut);
            }
            if kill_reason.is_some() {
                signal_group(pid, Signal::Term);
                asked_at = Some(Instant::now());
            }
        } else if asked_at.is_some_and(|at| at.elapsed() >= KILL_GRACE) {
            // The grace ran out with the child still here: insist. Clearing
            // `asked_at` is what keeps this to one escalation --- the loop
            // keeps spinning until `try_wait` reports the exit.
            asked_at = None;
            signal_group(pid, Signal::Kill);
            let _ = child.kill();
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

/// What [`signal_group`] sends. Two values, because cancellation is two
/// phases: ask, then insist (see [`supervise`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    /// "Wind up." Handled by anything that cleans up after itself --- and
    /// the reason the escalation exists at all.
    Term,
    /// "Stop now." Unblockable, so it is the second phase, never the first.
    Kill,
}

/// Signals the child's *whole process group*, not just the child.
///
/// A bare [`std::process::Child::kill`] reaches the one pid, and a tool like
/// `west` immediately delegates to helpers (cmake, ninja, the dashboard
/// generator) that keep running --- and keep the pipes open --- after their
/// parent dies: `Stop` reported the dashboard cancelled while it was still
/// being built in the background. Every child is spawned as its own group
/// leader (`Command::to_std`'s `process_group(0)`; portable-pty's `setsid`
/// for a PTY child), so its pid *is* the pgid and `kill(-pgid)` reaches the
/// tree. Already gone fails harmlessly with `ESRCH` --- a natural exit
/// racing the cancel is not an error.
///
/// **Known limitation.** The group is the whole tree, including anything the
/// tool launched *for the user*: `west build -t dashboard` opens its HTML
/// report in a browser, and a browser started from scratch (rather than
/// handing off to a running instance) is a descendant. Stopping the command
/// while `west` still waits on that launcher ends it too. It is signalled
/// [`Signal::Term`] first, which is what a browser needs to shut down
/// cleanly, but it is not spared. Narrowing the kill would need a way to
/// tell "helper the command owns" from "program the command handed the
/// user", which nothing here has.
#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return;
    };
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: `kill` takes no pointers and cannot touch this process's
    // memory. A negative pid addresses the group; every failure mode
    // (ESRCH, EPERM) is a no-op we deliberately ignore.
    unsafe {
        libc::kill(-pgid, signal);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: Signal) {}

/// Whether anything is left in `pid`'s process group --- signal 0 is the
/// standard "check, don't send" probe. A group leader that has exited but
/// has not been reaped yet still answers yes, which is correct for
/// [`ProcessManager::shutdown`]'s purpose: the supervisor threads are still
/// running during a drop and reap within [`POLL_INTERVAL`], so the wait
/// ends as soon as the child is really gone.
#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: as in `signal_group`; signal 0 sends nothing at all.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Without process groups there is nothing to wait for: `shutdown` skips
/// straight to the direct kill.
#[cfg(not(unix))]
fn group_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pty_command_is_its_program_and_arguments() {
        let builder = pty_command(&Command::new("/bin/sh").arg("-c").arg("true"));
        assert_eq!(
            builder.get_argv().as_slice(),
            [
                std::ffi::OsString::from("/bin/sh"),
                "-c".into(),
                "true".into()
            ]
        );
        assert!(!builder.is_default_prog());
    }

    #[test]
    fn a_login_shell_command_maps_to_the_default_program() {
        // Empty argv is portable-pty's grammar for "the user's shell, as a
        // login shell": it resolves and execs the shell itself, with the
        // leading-dash `argv[0]` the login convention rides on.
        let builder = pty_command(&Command::new("zsh").as_login_shell());
        assert!(builder.is_default_prog());
    }
}
