//! Capturing the board's own IPv4 address live, over its platform
//! monitor, instead of asking the user to read it off a display and type
//! it in.
//!
//! Structurally this is [`super::version_capture`] with a different
//! reader: a short-lived, self-closing PTY session on the board's own
//! monitor ([`crate::backend::Backend::monitor_command`]), which reboots
//! the board once the tool says it has the port and then scans every
//! chunk --- here with [`crate::ota::address::from_console`] instead of
//! [`crate::firmware_id::version`]. The two share the trick because they
//! answer the same shape of question: *the board already says this at
//! every boot; be attached when it does.*
//!
//! What differs is the **wait**. A boot banner is printed in the first
//! second; an address is not printed until the interface is up, the radio
//! has associated and DHCP has answered --- seconds later, and how many
//! is the network's business, not the firmware's. So the ceiling here is
//! much larger than the banner capture's 15s, and unlike that one it is
//! *not* a measured figure: it is a bound on someone else's DHCP server.
//! It costs nothing when the address arrives early, because a match ends
//! the session immediately.
//!
//! Unlike the version capture this one is **not** background courtesy
//! work: the user pressed something to start it, so it says so in the log
//! and reports a miss. It still never touches focus, the log tab or the
//! monitor source --- the interactive Monitor tab
//! ([`super::App::open_monitor`]'s `device_monitor_process`) stays the
//! user's.

use std::time::Duration;

use crate::console::LineConsole;
use crate::process::ProcessId;

use super::App;

/// How long the capture may hold the port before giving up.
///
/// The board has to boot, associate and get a lease inside this, and the
/// only one of those ChipTUI can time is the boot. 60s is chosen to be
/// longer than a slow association plus a retried DHCP exchange rather
/// than measured against one network's --- a ceiling, spent only when the
/// address never comes: [`App::on_address_capture_output`] ends the
/// session on the line that names it.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(60);

/// idf_monitor's own line announcing it has the port --- the signal that
/// the reset below has somewhere to land. Same anchor and same reason as
/// [`super::version_capture`]'s.
const MONITOR_READY: &str = "idf_monitor on";

/// idf_monitor's "reset the board": its menu key (`Ctrl+T`) then
/// `Ctrl+R`. The board has to *reboot* for the address lines to be
/// printed again --- a board that has been up for an hour already printed
/// them and said nothing since.
const RESET_COMMAND: [u8; 2] = [0x14, 0x12];

/// One in-flight address capture.
pub struct AddressCapture {
    /// The PTY process id, matched by `App::on_process`'s guards.
    pub(super) process: ProcessId,
    console: LineConsole,
    lines: Vec<String>,
    /// Whether [`RESET_COMMAND`] has been sent: once per capture, never
    /// on every chunk the reboot itself prints.
    reset_sent: bool,
    /// Whether an address was found, so the exit can tell a miss from a
    /// success without re-reading the transcript.
    found: bool,
}

impl AddressCapture {
    fn new(process: ProcessId) -> Self {
        Self {
            process,
            console: LineConsole::new(),
            lines: Vec::new(),
            reset_sent: false,
            found: false,
        }
    }
}

impl App {
    /// Starts the capture for the selected port. Returns whether it
    /// started; `false` means the caller falls back to asking for the
    /// address by hand in the same keypress --- the platform monitor's own
    /// prerequisites (a resolved workspace, an Espressif board, a
    /// configured build directory) are not this feature's to invent.
    ///
    /// A capture already running is not restarted: it holds the port, and
    /// a second one would only fail to open it.
    pub fn start_address_capture(&mut self) -> bool {
        if self.address_capture.is_some() {
            return true;
        }
        let facts = self.monitor_facts();
        let Some(backend) = self.manager.backend() else {
            return false;
        };
        let Ok(command) = backend.monitor_command(&facts.context()) else {
            return false;
        };
        match self.processes.spawn_pty(command, CAPTURE_TIMEOUT) {
            Ok(process) => {
                // The user asked for this, so it is announced --- unlike
                // the boot-banner capture, which is background work and
                // stays silent. The sentence names the reset, because a
                // board that was running is about to stop.
                self.logs.info(
                    "OTA: reading the board's address --- restarting it and \
                     watching the console",
                );
                self.address_capture = Some(AddressCapture::new(process));
                true
            }
            Err(err) => {
                self.logs
                    .warn(format!("OTA: cannot read the address: {err}"));
                false
            }
        }
    }

    /// Feeds one chunk of capture output, recording the address and
    /// closing the session the moment a line names it.
    pub(super) fn on_address_capture_output(&mut self, text: &str) {
        let Some(mut capture) = self.address_capture.take() else {
            return;
        };
        capture.console.feed(&mut capture.lines, text);
        let transcript = capture.lines.join("\n");
        let reset = !capture.reset_sent && transcript.contains(MONITOR_READY);
        if reset {
            capture.reset_sent = true;
        }
        let process = capture.process;
        let address = crate::ota::address::from_console(&transcript);
        capture.found = address.is_some();
        self.address_capture = Some(capture);
        if reset {
            self.processes.write_stdin(process, &RESET_COMMAND);
        }
        let Some(address) = address else {
            return;
        };
        // idf_monitor's own exit key hangs on kernels without TIOCSTI, so
        // the session is stopped from the host side --- the same rule
        // `ctrl+]` follows on the interactive Monitor tab.
        self.processes.cancel(process);
        self.apply_captured_address(address);
    }

    /// Records a captured address as the answer, exactly as the typed one
    /// is recorded: through the panel, which persists `[ota] address`.
    fn apply_captured_address(&mut self, address: String) {
        let Some(panel) = &mut self.ota else {
            return;
        };
        match panel.set_address(address.clone()) {
            Ok(()) => self
                .logs
                .info(format!("OTA: the board answers at {address}")),
            Err(err) => self
                .logs
                .error(format!("could not record the address: {err}")),
        }
    }

    /// The capture's process exited: a match already cancelled it, or the
    /// ceiling did. A miss is reported --- the user asked, so silence
    /// would leave a window that did nothing and said nothing.
    pub(super) fn finish_address_capture(&mut self) {
        let Some(capture) = self.address_capture.take() else {
            return;
        };
        if capture.found {
            return;
        }
        self.logs.warn(
            "OTA: the board printed no address --- turn on the address log in the \
             prepare step, or type it by hand",
        );
    }
}
