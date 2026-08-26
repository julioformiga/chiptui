//! Capturing a Zephyr board's boot banner live, over its own platform
//! monitor, instead of guessing which byte range of flash it printed into.
//!
//! `esptool` resets the board back into run mode once the identification
//! read finishes (its default `--after hard-reset`), so a Zephyr *simple
//! boot* image that named itself without a version reboots and prints its
//! `*** Booting Zephyr OS build vX.Y.Z ***` banner on the UART shortly
//! after --- regardless of how big the image is or where the banner
//! physically sits in flash (unlike [`crate::firmware_id`]'s fixed-size
//! `HUNT_SIZE`, which any large enough app outgrows). This mirrors
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
/// and the board must reboot again (past the identification read's own
/// reset) before it reaches its own banner print --- 15s gives both enough
/// headroom without holding the port so long a miss reads as a hang.
/// Unlike the probe, there is no "idle vs. running" ambiguity to resolve
/// early: only "found the banner" or "didn't," so one hard cap is enough.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// One in-flight live version capture.
pub struct FirmwareVersionCapture {
    /// The PTY process id, matched by `App::on_process`'s guards.
    pub(super) process: ProcessId,
    console: LineConsole,
    lines: Vec<String>,
}

impl FirmwareVersionCapture {
    fn new(process: ProcessId) -> Self {
        Self {
            process,
            console: LineConsole::new(),
            lines: Vec::new(),
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
        let version =
            firmware_id::version(capture.lines.join("\n").as_bytes(), FlashFirmware::Zephyr);
        self.version_capture = Some(capture);
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
