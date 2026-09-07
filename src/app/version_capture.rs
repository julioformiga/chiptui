//! Capturing a Zephyr board's boot banner live, over its own platform
//! monitor, instead of guessing which byte range of flash it printed into.
//!
//! A Zephyr image that named itself without a version prints its `***
//! Booting Zephyr OS build vX.Y.Z ***` banner on the UART every time it
//! boots --- regardless of how big the image is or where the banner
//! physically sits in flash (unlike [`crate::firmware_id`]'s fixed-size
//! `HUNT_SIZE`, which any large enough app outgrows). Catching it means
//! *being attached when it prints*, which is why the capture reboots the
//! board itself once the monitor is up ([`RESET_COMMAND`]): `esptool`
//! does reset the board back into run mode when the identification read
//! finishes (its default `--after hard-reset`), but that boot is over ---
//! banner and all --- seconds before `west`, Python and idf_monitor have
//! finished starting, and a board whose application only speaks at boot
//! then says nothing for as long as the capture is willing to wait
//! (measured on an ESP32-C3: attached, silent; reset from inside the
//! monitor, banner in 1.1s). This mirrors
//! [`super::probe::DeviceProbe`]'s trick for MicroPython's REPL banner,
//! reusing the board's own platform monitor (`west espressif monitor`)
//! instead of `mpremote repl`: a short-lived, self-closing PTY session,
//! never the interactive Monitor tab ([`super::App::open_monitor`]'s
//! `device_monitor_process`) --- this must stay invisible background
//! courtesy work, the same rule the chip-id/firmware-read queries already
//! follow, so it never touches focus, the log tab or the monitor source.
//!
//! Only runs when [`crate::backend::MonitorContext`]'s own prerequisites
//! are already met (a resolved workspace, a known Espressif board, a
//! configured build directory); when they aren't, or the capture times out
//! without a match, [`crate::flash::FlashPanel::query_firmware_version`]'s
//! flash-byte hunt remains the fallback --- this is a hybrid, not a
//! replacement, since identification must keep working with nothing but a
//! selected port.

use std::time::Duration;

use crate::console::LineConsole;
use crate::firmware_id::{self, FlashFirmware};
use crate::process::ProcessId;

use super::App;

/// How long the capture may hold the port before giving up. `west
/// espressif monitor` wraps idf_monitor, which is observably slower to
/// reach a responsive state than `mpremote repl`'s near-instant connect,
/// and the board only reboots once [`RESET_COMMAND`] reaches it, one
/// round trip past that --- 15s gives the whole sequence headroom without
/// holding the port so long a miss reads as a hang.
/// Unlike the probe, there is no "idle vs. running" ambiguity to resolve
/// early: only "found the banner" or "didn't," so one hard cap is enough.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// idf_monitor's own line announcing that it has the port, printed
/// before it starts reading the keyboard --- the signal that the reset
/// below has somewhere to land. Waiting for the tool to say so beats
/// sleeping for a guessed interval, and it also scopes the reset to
/// idf_monitor alone: `monitor_command`'s mpremote fallback prints no
/// such line and is left untouched.
const MONITOR_READY: &str = "idf_monitor on";

/// idf_monitor's "reset the board" command: its menu key (`Ctrl+T`)
/// followed by `Ctrl+R`, which toggles RTS the way `esptool` does. Sent
/// as bytes into the session's PTY, exactly as the Monitor tab forwards
/// the same chord when a user types it (`monitor_control_keys_reach_the
/// _session_as_their_bytes`). The board is reset, not interrupted
/// silently: the identification question the user answered yes to
/// ("stops it and reads its data") is the authorization this whole chain
/// rides on, and `esptool` has already reset the board twice under it by
/// the time the capture starts.
const RESET_COMMAND: [u8; 2] = [0x14, 0x12];

/// One in-flight live version capture.
pub struct FirmwareVersionCapture {
    /// The PTY process id, matched by `App::on_process`'s guards.
    pub(super) process: ProcessId,
    console: LineConsole,
    lines: Vec<String>,
    /// Whether [`RESET_COMMAND`] has been sent: the board is rebooted
    /// once per capture, never on every chunk the reboot itself prints.
    reset_sent: bool,
}

impl FirmwareVersionCapture {
    fn new(process: ProcessId) -> Self {
        Self {
            process,
            console: LineConsole::new(),
            lines: Vec::new(),
            reset_sent: false,
        }
    }
}

impl App {
    /// Opens the capture for the selected port, if the platform monitor can
    /// actually run for this board right now. Returns whether it started;
    /// `false` (missing workspace/board/build dir, a non-Espressif board,
    /// or a spawn failure) means the caller should fall back to the
    /// flash-byte hunt in the same tick.
    pub(super) fn start_version_capture(&mut self) -> bool {
        let facts = self.monitor_facts();
        let context = facts.context();
        let Some(backend) = self.manager.backend() else {
            return false;
        };
        let Ok(mut command) = backend.monitor_command(&context) else {
            return false;
        };
        // The mpremote override seam `open_monitor` also applies: Zephyr's
        // own `monitor_command` falls back to `mpremote repl` when the
        // board is auto-detected as MicroPython, and that invocation must
        // run through the browser's own tool path like every other one.
        if command.program() == crate::backend::micropython::commands::PROGRAM
            && let Some(tool) = self
                .browser
                .as_ref()
                .and_then(crate::browser::Browser::tool_path)
        {
            command = command.with_program(tool.to_string());
        }
        match self.processes.spawn_pty(command, CAPTURE_TIMEOUT) {
            // Background courtesy work: no log line on start, same silence
            // as the byte hunt it stands in for.
            Ok(process) => {
                self.version_capture = Some(FirmwareVersionCapture::new(process));
                true
            }
            Err(_) => false,
        }
    }

    /// Feeds one chunk of capture output, applying the version and closing
    /// the session the moment the banner names it.
    pub(super) fn on_version_capture_output(&mut self, text: &str) {
        let Some(mut capture) = self.version_capture.take() else {
            return;
        };
        capture.console.feed(&mut capture.lines, text);
        let transcript = capture.lines.join("\n");
        // The board boots on the monitor's own reset, so the banner can
        // only appear after this: the version read below finds nothing
        // until the chunk that carries it arrives.
        let reset = !capture.reset_sent && transcript.contains(MONITOR_READY);
        if reset {
            capture.reset_sent = true;
        }
        let process = capture.process;
        let version = firmware_id::version(transcript.as_bytes(), FlashFirmware::Zephyr);
        self.version_capture = Some(capture);
        if reset {
            self.processes.write_stdin(process, &RESET_COMMAND);
        }
        let Some(version) = version else {
            return;
        };
        // idf_monitor's own exit key hangs on kernels without TIOCSTI (the
        // same reason `ctrl+]` stops the interactive Monitor tab from the
        // host side rather than writing an escape byte): SIGTERM to the
        // process group, not a written byte.
        if let Some(capture) = self.version_capture.as_ref() {
            self.processes.cancel(capture.process);
        }
        if let Some(mut flash) = self.flash.take() {
            if let Some(notice) = flash.apply_live_zephyr_version(version) {
                self.logs.push(notice.0, notice.1);
            }
            self.flash = Some(flash);
        }
    }

    /// The capture's process exited (a match already cancelled it, or the
    /// hard timeout did): release the port. No log on a clean miss --- "a
    /// failed hunt changes nothing" applies here exactly as it does to
    /// [`crate::flash::FlashPanel::apply_version_from`].
    pub(super) fn finish_version_capture(&mut self) {
        self.version_capture = None;
    }
}
