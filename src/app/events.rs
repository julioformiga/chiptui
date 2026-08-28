//! The event loop's entry point and the two long dispatches behind it.
//!
//! [`App::handle`] is the single door every event comes through;
//! [`App::on_process`] fans a `ProcessEvent` out to whichever subsystem
//! owns that process id, and is where the identification chain (probe →
//! chip query → firmware read → version) advances one link at a time; and
//! [`App::check_device_hotplug`] is the once-a-second `/dev` poll that
//! notices a board arriving or leaving. Nothing here decides *what* a
//! subsystem does with its event --- it decides only which one is asked.

use crate::backend::Capability;
use crate::browser::Browser;
use crate::event::AppEvent;
use crate::flash::FlashPanel;

use super::{App, Overlay, RunState, View, flash_view};

impl App {
    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.on_key(key),
            // Ratatui re-renders from scratch each frame, so a resize only has
            // to invalidate what depends on the old geometry.
            // A paste belongs to whatever owns the keyboard, and only the
            // shell does: nothing else on the dashboard reads text.
            AppEvent::Paste(text) => {
                if self.is_terminal_active() {
                    self.paste_into_terminal(&text);
                }
            }
            AppEvent::Resize { .. } => self.logs.scroll_to_bottom(),
            // A mouse gesture is only ever an alternative trigger for an
            // action the keyboard already owns, and only when the session
            // asked for reporting --- otherwise it is dropped here, before
            // any handler sees it.
            AppEvent::Mouse(event) => {
                if self.mouse_enabled {
                    self.on_mouse(event);
                }
            }
            AppEvent::Tick => {
                self.ticks = self.ticks.wrapping_add(1);
                self.check_device_hotplug();
                self.refresh_local_listings();
                self.refresh_requirements();
                self.tick_probe();
                self.maybe_ask_identification();
                self.check_interrupt_gate();
                self.maybe_offer_restore();
                self.maybe_run_deferred_firmware_check();
                self.maybe_run_deferred_version_hunt();
                if self.maybe_run_deferred_flash_query() == flash_view::DeferredQuery::Dropped {
                    // The held listing was waiting on a query that can never
                    // start now; listing beats waiting forever.
                    self.drive_held_root_listing();
                }
                self.drive_held_root_listing();
                // The board/shield pickers' documentation fetches: whatever
                // row the cursor rests on is the one whose picture and
                // details are worth fetching, debounced by the tick.
                self.drive_docs_selection();
            }
            AppEvent::Process(event) => self.on_process(&event),
            AppEvent::Docs(event) => {
                if let Some((level, message)) = self.docs.apply(event) {
                    match level {
                        crate::logs::Level::Error => self.logs.error(message),
                        crate::logs::Level::Warn => self.logs.warn(message),
                        _ => self.logs.info(message),
                    }
                }
            }
        }
    }

    pub(super) fn check_device_hotplug(&mut self) {
        // Every device-shaped backend watches for connect/disconnect: one
        // with a filesystem rescans through `mpremote devs`, a monitor-only
        // one (Zephyr) through the same `/dev` walk its scan is. A backend
        // with neither has no device status worth keeping fresh.
        let caps = self.manager.capabilities();
        if !caps.contains(Capability::Filesystem) && !caps.contains(Capability::Monitor) {
            return;
        }
        // only check every 4 ticks (1 second)
        if !self.ticks.is_multiple_of(4) {
            return;
        }
        // A rescan can open the device picker (several boards appeared) or
        // clear the selection (they all left); neither belongs on top of a
        // dialog the user is mid-answer on. The count comparison simply
        // happens on a later tick once it closes.
        if self.overlay.is_some() {
            return;
        }

        if matches!(
            self.devices.discovery,
            crate::device::DiscoveryState::Scanning
        ) {
            return;
        }
        if self.browser.as_ref().is_some_and(Browser::is_busy) {
            return;
        }
        // A probe holds the port exactly like a listing does, and a scan
        // racing it would ask the same serial port two questions at once.
        if self.probe.is_some() {
            return;
        }
        if self.device_monitor_process.is_some() {
            return;
        }
        if self.run_process.is_some() {
            return;
        }
        // esptool resets the board to read its chip and firmware, which
        // drops a native-USB board's tty node for the reset window: acting
        // on that as a real disconnect wipes the identification the chain
        // is mid-flight on (`tests/device_hotplug.rs`'s port-blip case).
        // The baseline still tracks the port count either way --- freezing
        // it too would blind the very next poll after the query settles,
        // which is exactly when a *real* disconnect during the query would
        // otherwise go uncaught (`hotplug_updates_the_device_status`, whose
        // disconnect lands the instant the identification read finishes).
        let flash_busy = self.flash.as_ref().is_some_and(FlashPanel::is_busy);

        let current_count = crate::device::usb_serial_ports(&self.serial_dir).len();
        if let Some(last) = self.last_port_count
            && current_count != last
            && !flash_busy
        {
            self.logs
                .info("device connection change detected, rescanning...");
            if caps.contains(Capability::Filesystem) {
                self.scan_devices();
            } else {
                self.scan_serial_devices();
            }
        }
        self.last_port_count = Some(current_count);
    }

    /// Routes a process result to whatever asked for it.
    ///
    /// Both subsystems see every event: each guards on its own in-flight
    /// process id and is a no-op for an event it did not start, which is
    /// simpler than tracking ownership here.
    pub(super) fn on_process(&mut self, event: &crate::process::ProcessEvent) {
        // The package index fetch owns its events before any subsystem:
        // curl shares the process pool with mpremote/esptool, and an
        // unrecognized id must still reach whoever it belongs to.
        if self.on_package_index_process(event) {
            return;
        }
        match event {
            // The script-probe session (PTY): raw output arrives as bytes, and
            // its exit is what releases the port for the listing it deferred.
            crate::process::ProcessEvent::Output { id, text }
                if self
                    .probe
                    .as_ref()
                    .is_some_and(|probe| probe.process == *id) =>
            {
                self.on_probe_output(text);
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome: _,
                duration: _,
            } if self
                .probe
                .as_ref()
                .is_some_and(|probe| probe.process == *id) =>
            {
                self.finish_probe();
                return;
            }
            // The live Zephyr boot-banner capture (PTY): decoded output is
            // scanned for the banner after every chunk, and its exit (found
            // or timed out) is what releases the port back to the deferred
            // version hunt's own fallback.
            crate::process::ProcessEvent::Output { id, text }
                if self
                    .version_capture
                    .as_ref()
                    .is_some_and(|capture| capture.process == *id) =>
            {
                self.on_version_capture_output(text);
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome: _,
                duration: _,
            } if self
                .version_capture
                .as_ref()
                .is_some_and(|capture| capture.process == *id) =>
            {
                self.finish_version_capture();
                return;
            }
            crate::process::ProcessEvent::Line {
                id,
                stream: _,
                text,
            } if Some(*id) == self.device_monitor_process => {
                self.monitor_console
                    .push_line(&mut self.device_monitor_output, text.clone());
                self.update_script_from_monitor();
                return;
            }
            crate::process::ProcessEvent::Output { id, text }
                if Some(*id) == self.device_monitor_process =>
            {
                self.monitor_console
                    .feed(&mut self.device_monitor_output, text);
                self.update_script_from_monitor();
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome,
                duration: _,
            } if Some(*id) == self.device_monitor_process => {
                self.device_monitor_process = None;
                self.monitor_console.push_line(
                    &mut self.device_monitor_output,
                    format!("[monitor {}]", outcome.summary()),
                );
                return;
            }
            // The Terminal tab's shell (PTY): the emulator is fed the raw
            // bytes --- decoding them per read chunk is what turns a
            // powerline glyph straddling the boundary into U+FFFD. Its exit
            // frees the keyboard the way the monitor's does.
            crate::process::ProcessEvent::Bytes { id, data }
                if Some(*id) == self.terminal_process =>
            {
                self.feed_terminal(data);
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome,
                duration: _,
            } if Some(*id) == self.terminal_process => {
                self.terminal_process = None;
                self.terminal_detached = false;
                // Written *through* the emulator, so the epitaph lands in
                // the grid wherever the shell left the cursor --- CR before
                // LF because the grid is a terminal, not a line list.
                self.terminal
                    .write(&format!("\r\n[shell {}]\r\n", outcome.summary()));
                return;
            }
            // Run session (PTY): streamed output arrives as raw bytes.
            crate::process::ProcessEvent::Output { id, text } if Some(*id) == self.run_process => {
                self.run_console.feed(&mut self.run_output, text);
                return;
            }
            crate::process::ProcessEvent::Finished {
                id,
                outcome,
                duration: _,
            } if Some(*id) == self.run_process => {
                self.run_process = None;
                self.run_state = RunState::Finished;
                self.run_console
                    .push_line(&mut self.run_output, format!("[run {}]", outcome.summary()));
                // The run *was* the user code: finished means the device sits
                // at its REPL with nothing executing.
                self.set_script_state(crate::device::ScriptState::Stopped);
                return;
            }
            _ => {}
        }

        if let Some(mut browser) = self.browser.take() {
            let port = self.devices.selected_port().map(str::to_string);
            let update = browser.on_process(event, &mut self.processes, port.as_deref());
            self.browser = Some(browser);

            for (level, message) in update.notices {
                self.logs.push(level, message);
            }
            match update.device_scan {
                Some(Ok(devices)) => {
                    let empty = devices.is_empty();
                    self.devices.set_devices(devices);

                    if empty {
                        self.set_device_pane_error(
                            "no MicroPython device found — connect a board and press 'd'",
                        );
                        self.device_disconnected();
                    } else if self.devices.needs_selection() {
                        // Several boards: ask before touching any of them.
                        self.open_device_picker();
                    } else {
                        // Exactly one, or a previous choice still present.
                        // The identification question is armed *before* the
                        // listing is requested: nothing identification-shaped
                        // may touch the port without the user's answer, and
                        // arming after would let the listing win the port
                        // before the question even opened.
                        self.request_device_identify();
                        self.load_device_root();
                    }
                }
                Some(Err(error)) => {
                    self.devices.set_failed(error.clone());
                    self.set_device_pane_error(error);
                    self.device_disconnected();
                }
                None => {}
            }
            if let Some(view) = update.device_view {
                self.apply_device_view(view);
            }
            if update.prompt_micropython_flash
                && !matches!(
                    self.overlay,
                    Some(Overlay::ConfirmEraseForMicroPython { .. })
                )
            {
                self.overlay = Some(Overlay::ConfirmEraseForMicroPython { confirm: false });
            }
            if let Some(running) = update.script_running {
                self.set_script_state(if running {
                    crate::device::ScriptState::Running
                } else {
                    crate::device::ScriptState::Stopped
                });
            }
            if let Some(transfer) = update.transfer {
                self.apply_transfer(transfer);
            }
            if let Some(plan) = update.sync_plan {
                self.overlay = Some(Overlay::SyncPreview {
                    plan,
                    confirm: false,
                });
            }
            if let Some(path) = update.listed {
                self.on_device_listing(&path);
            }
        }

        self.install_on_process(event);

        if let Some(mut build) = self.build.take() {
            let caps = self.manager.capabilities();
            let notices = build.on_process(event, &caps);
            // The flash contents may have just changed under a build-panel
            // command; read the flag before the panel goes back.
            let flashed = build.take_flash_finished();
            // The build dashboard closed itself to run this; it comes back
            // on the tab that asked, with the fresh report loaded.
            let reported = build.take_size_report_finished();
            self.build = Some(build);
            for (level, message) in notices {
                self.logs.push(level, message);
            }
            if flashed {
                self.reidentify_firmware_after_flash();
            }
            if reported {
                self.reopen_dashboard_on_memory();
            }
        }

        if let Some(mut flash) = self.flash.take() {
            let update = flash.on_process(event);
            let fetch_update = flash.on_curl_process(event);
            let chip_query_finished = update.background_chip_query_finished;
            let firmware_read_finished = update.background_firmware_read_finished;
            self.flash = Some(flash);

            for (level, message) in update.notices {
                self.logs.push(level, message);
            }
            if update.offer_flash {
                self.offer_flash_after_erase();
            }
            if update.search_online_for_firmware {
                self.search_online();
                self.view = View::Flash;
            }

            for (level, message) in fetch_update.notices {
                self.logs.push(level, message);
            }
            if fetch_update.download_finished {
                self.offer_flash_after_download();
            }

            // An erase or write-flash changed what the flash carries: the
            // verdict that gated the listing is as stale as the flash it
            // was read from, so it gets the same reload `west flash` gets
            // --- dropped and re-armed, a new identification running on
            // its own once the port frees, rather than waiting for the
            // next listing to re-ask.
            if update.firmware_invalidated {
                self.reidentify_firmware_after_flash();
            }
            if firmware_read_finished {
                self.firmware_check = flash_view::FirmwareCheck::Idle;
                // A foreign firmware (or a blank chip) means there was
                // never a MicroPython script to bring back: the "running
                // script" the probe saw was the firmware printing its
                // boot banner, so the restore question that an accepted
                // interruption armed has nothing to offer.
                let foreign = self
                    .flash
                    .as_ref()
                    .and_then(|flash| flash.details.firmware.clone())
                    .is_some_and(|verdict| {
                        !matches!(
                            verdict,
                            crate::firmware_id::FirmwareVerdict::Firmware(
                                crate::firmware_id::FlashFirmware::MicroPython,
                                _
                            )
                        )
                    });
                if foreign {
                    self.restore_pending = false;
                }
            }
            // The background chip query gates the first device listing on
            // a newly selected device; its success arms the firmware read
            // the listing waits on next, and either finishing re-drives
            // the chain.
            if chip_query_finished {
                self.arm_firmware_check();
            }
            if chip_query_finished || firmware_read_finished {
                self.drive_held_root_listing();
            }
        }

        // The deferred query (above, and from `apply_device_picker`) can only
        // start once every port holder is gone and the user has nothing to
        // answer --- checked here for promptness and again on every tick,
        // because with a held queue (a running script) no process event ever
        // arrives to reach this line.
        if self.maybe_run_deferred_flash_query() == flash_view::DeferredQuery::Dropped {
            self.drive_held_root_listing();
        }
        self.maybe_run_deferred_firmware_check();
        self.maybe_run_deferred_version_hunt();
        self.drive_held_root_listing();

        // A completed command may have armed either follow-up question.
        self.maybe_ask_identification();
        self.check_interrupt_gate();
        self.maybe_offer_restore();
    }
}
