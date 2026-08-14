//! The device script probe: a short `mpremote repl` session run before the
//! first filesystem operation on a newly selected device.
//!
//! `mpremote` interrupts whatever is running (Ctrl-C, then raw REPL) for
//! *every* `fs`/`exec`/`df` command, so a board executing a blocking
//! `main.py` loop has its script silently killed by the first listing. The
//! serial port offers no non-invasive query for "is user code running", but a
//! REPL connection --- which sends nothing unless asked --- shows exactly
//! what a human plugging in would see: a `>>> ` prompt means idle, a stream
//! of output with no prompt means a script.
//!
//! So before that first interrupting command, ChipTUI opens `mpremote repl`
//! in a PTY for a moment, classifies what it sees
//! ([`crate::device::monitor_script_activity`]), closes the session
//! (ctrl-], mpremote's own escape --- no reset, no interrupt), and only then
//! proceeds. A board believed busy has its device operations held behind a
//! confirmation instead ([`crate::browser::Browser::set_interrupt_gate`]).
//!
//! Honest limits, by design: a script that never prints is indistinguishable
//! from an idle-but-quiet board, so the probe can return "unknown" and the
//! first command proceeds ungated --- the pre-probe behavior. Boards that
//! reset on serial open (classic ESP32 auto-reset circuits) reboot when the
//! probe connects, which restarts `main.py`; the probe then correctly sees a
//! running script.

use std::time::{Duration, Instant};

use crate::backend::micropython::commands;
use crate::browser::Browser;
use crate::console::LineConsole;
use crate::device::ScriptState;
use crate::process::ProcessId;

use super::App;

/// How long the probe process may live at all; it normally exits long before
/// after the probe concludes and closes it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// How long the probe may watch before concluding "unknown". Wall-clock
/// rather than tick-counted: an idle board shows its prompt within a few
/// hundred milliseconds and a printing script crosses the line equally fast,
/// so 3s of silence is genuinely inconclusive, whatever the tick rate.
const PROBE_DECISION_WINDOW: Duration = Duration::from_secs(3);

/// mpremote's repl escape, which quits the session client-side without
/// touching the device.
const CTRL_RIGHT_BRACKET: u8 = 0x1d;

/// One in-flight probe session.
pub struct DeviceProbe {
    /// The PTY process id, matched by `App::on_process`'s guards.
    pub(super) process: ProcessId,
    console: LineConsole,
    lines: Vec<String>,
    started: Instant,
    /// The verdict, once reached; the process is being closed after this.
    concluded: Option<Option<ScriptState>>,
}

impl DeviceProbe {
    fn new(process: ProcessId) -> Self {
        Self {
            process,
            console: LineConsole::new(),
            lines: Vec::new(),
            started: Instant::now(),
            concluded: None,
        }
    }
}

impl App {
    /// Opens the probe on `port`, if one is warranted right now.
    ///
    /// Returns `true` when a probe started, meaning the caller must wait for
    /// [`Self::finish_probe`] before issuing device commands --- the probe
    /// holds the serial port. Only runs while the script state is unknown,
    /// once per selected port, and never alongside the monitor, a run session
    /// or an in-flight device command (all of which already hold the port).
    pub(super) fn start_device_probe(&mut self, port: Option<&str>) -> bool {
        let Some(port) = port else { return false };
        // No browser means no filesystem commands are coming: nothing for a
        // probe to protect (a flash-only backend selects devices too).
        if self.browser.is_none()
            || self.devices.script_state() != ScriptState::Unknown
            || self.probe.is_some()
            || self.probed_port.as_deref() == Some(port)
            || self.device_monitor_process.is_some()
            || self.run_process.is_some()
            || self.browser.as_ref().is_some_and(Browser::is_busy)
        {
            return false;
        }
        let tool = self.browser.as_ref().and_then(Browser::tool_path);
        let mut command = commands::repl(Some(port));
        if let Some(tool) = tool {
            command = command.with_program(tool.to_string());
        }
        match self.processes.spawn_pty(command, PROBE_TIMEOUT) {
            Ok(process) => {
                self.probed_port = Some(port.to_string());
                self.probe = Some(DeviceProbe::new(process));
                self.logs
                    .info(format!("checking whether {port} is running a script"));
                true
            }
            Err(error) => {
                self.logs
                    .warn(format!("could not check for a running script: {error}"));
                false
            }
        }
    }

    /// Feeds one chunk of probe output, concluding as soon as the output is
    /// decisive; a concluded probe closes itself (ctrl-]), and its Finished
    /// event carries the deferral onward.
    pub(super) fn on_probe_output(&mut self, text: &str) {
        let Some(mut probe) = self.probe.take() else {
            return;
        };
        probe.console.feed(&mut probe.lines, text);
        let verdict = crate::device::monitor_script_activity(&probe.lines);
        if verdict.is_some() && probe.concluded.is_none() {
            probe.concluded = Some(verdict);
            self.processes
                .write_stdin(probe.process, &[CTRL_RIGHT_BRACKET]);
        }
        self.probe = Some(probe);
    }

    /// Gives up on an indecisive probe once its window has passed --- a
    /// silent board tells the user nothing either way, and holding the port
    /// longer just delays the listing. Ticks are the polling heartbeat, not
    /// the clock: the window itself is wall-clock ([`PROBE_DECISION_WINDOW`]).
    pub(super) fn tick_probe(&mut self) {
        let conclude = self.probe.as_ref().is_some_and(|probe| {
            probe.concluded.is_none() && probe.started.elapsed() >= PROBE_DECISION_WINDOW
        });
        if conclude && let Some(mut probe) = self.probe.take() {
            probe.concluded = Some(None);
            self.processes
                .write_stdin(probe.process, &[CTRL_RIGHT_BRACKET]);
            self.probe = Some(probe);
        }
    }

    /// Applies the probe's verdict and releases the listing it deferred:
    /// `load_device_root` runs again here, now allowed to actually list. A
    /// "running" verdict turns the interrupt gate on first, so the listing
    /// waits for the user instead of silently stopping the script.
    pub(super) fn finish_probe(&mut self) {
        let Some(probe) = self.probe.take() else {
            return;
        };
        match probe.concluded.flatten() {
            Some(ScriptState::Running) => {
                self.logs.warn(
                    "a script is running on the device --- device operations will ask before interrupting",
                );
                self.set_script_state(ScriptState::Running);
            }
            Some(ScriptState::Stopped) => {
                self.logs
                    .info("device is idle at its REPL (no script running)");
                self.set_script_state(ScriptState::Stopped);
            }
            _ => {}
        }
        self.load_device_root();
        self.check_interrupt_gate();
    }
}
