//! Device discovery and selection: scanning for a board, the device picker,
//! and applying a backend choice (manual override or the empty-project
//! prompt) --- the moments that can make a device or a filesystem-capable
//! backend newly available. Split out of `app.rs` since these are the
//! handful of places that decide *which* device/backend `App` is talking to,
//! as opposed to what to do once one is chosen.

use std::path::{Path, PathBuf};

use crate::backend::{BackendKind, Capability};
use crate::browser::Browser;
use crate::device::ScriptState;

use super::{App, LogTab, MonitorSource, Overlay, PickerOption};

impl App {
    /// Ensures the file browser exists and, when the backend declares
    /// [`Capability::Filesystem`], starts a device scan --- without waiting
    /// for the user to move focus onto a file browser pane. The browser is
    /// created for every backend: its local pane is useful on its own
    /// (view/edit/delete work with no device filesystem); only the *scan*
    /// is capability-gated, and the device pane stays `Idle` without it.
    /// A no-op once a browser already exists (`AGENTS.md` §5's "one
    /// `mpremote` at a time" applies just as much to not re-issuing a scan
    /// that already ran).
    ///
    /// Called from three places: `main.rs` right after startup, and
    /// [`App::apply_project_setup`]/[`App::apply_picker`] --- any moment the
    /// selected backend could change what row 2 can show.
    /// Deliberately **not** called from [`App::bootstrap`] itself, for
    /// the same reason [`App::maybe_open_project_setup`] is not: many existing
    /// tests call `bootstrap()` directly and do not expect a subprocess to
    /// spawn as a side effect.
    pub fn maybe_scan_devices(&mut self) {
        self.ensure_browser_scanning();
        self.ensure_build_panel();
        self.maybe_scan_serial_ports();
    }

    /// The serial-port half of device discovery, for a backend with a
    /// monitor but no listing tool of its own (no `mpremote devs` to lean
    /// on): a plain `/dev` walk, synchronous because it is one cheap
    /// `read_dir` --- no subprocess to schedule. Fills the same `devices`
    /// state the mpremote scan does, so selection, the picker and the
    /// monitor all work identically afterwards.
    fn maybe_scan_serial_ports(&mut self) {
        let caps = self.manager.capabilities();
        if caps.contains(Capability::Filesystem) || !caps.contains(Capability::Monitor) {
            return;
        }
        self.scan_serial_devices();
    }

    /// Points the USB-serial scan at a directory other than `/dev` --- how
    /// tests make discovery deterministic without touching whatever happens
    /// to be plugged into the machine running them.
    pub fn set_serial_dir(&mut self, dir: impl Into<std::path::PathBuf>) {
        self.serial_dir = dir.into();
    }

    /// Scans `serial_dir` for USB serial ports and applies the result:
    /// one port selects itself, several ask, none reports why.
    pub fn scan_serial_devices(&mut self) {
        self.devices.set_scanning();
        let ports = crate::device::usb_serial_ports(&self.serial_dir);
        if ports.is_empty() {
            self.devices
                .set_failed("no USB serial port found — connect the board and press 'd'");
            return;
        }
        let devices = ports
            .into_iter()
            .map(|port| crate::device::DeviceInfo {
                port,
                serial: None,
                vid_pid: String::new(),
                description: "USB serial port".to_string(),
            })
            .collect();
        self.devices.set_devices(devices);
        if self.devices.needs_selection() {
            self.open_device_picker();
        }
    }

    /// Creates the browser (once), and starts a device scan with it when the
    /// backend has a filesystem to list. Only the listing itself waits for
    /// the scan to name a port: issuing the scan now would let mpremote
    /// auto-connect to whichever board answers first, which is the guess
    /// `SPEC.md` §8 forbids.
    fn ensure_browser_scanning(&mut self) {
        if self.browser.is_some() {
            return;
        }
        let root = self
            .manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
        self.browser = Some(Browser::new(Self::initial_local_dir(root)));
        if self.manager.capabilities().contains(Capability::Filesystem) {
            self.scan_devices();
        }
    }

    /// Creates the build panel, once, for the backend that shows it in row
    /// 2's right half (can build, no device filesystem). Like the browser's
    /// creation this reads no subprocess --- only the project's own
    /// `CMakeCache.txt`, if one exists --- so it is safe to call eagerly.
    fn ensure_build_panel(&mut self) {
        if !self.build_pane_visible_precondition() || self.build.is_some() {
            return;
        }
        let root = self
            .manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
        self.build = Some(crate::build::BuildPanel::new(root, self.logs.offset()));
    }

    /// The capability half of [`Self::build_pane_visible`]: whether a build
    /// panel belongs to this backend at all, independent of whether one
    /// exists yet (used before creating it, where `is_some` would always be
    /// false).
    pub(super) fn build_pane_visible_precondition(&self) -> bool {
        let caps = self.manager.capabilities();
        caps.contains(Capability::Build) && !caps.contains(Capability::Filesystem)
    }

    /// Files opens on `src/` when the project has one --- the directory kept
    /// in sync with the device (`SPEC.md` §9) --- so the local pane starts
    /// showing exactly what a `Filesystem` upload would send. Falls back to
    /// the project root for projects without a `src/` (Zephyr, or a
    /// MicroPython project that predates this layout).
    fn initial_local_dir(root: PathBuf) -> PathBuf {
        let src = root.join("src");
        if src.is_dir() { src } else { root }
    }

    pub(super) fn scan_devices(&mut self) {
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        self.devices.set_scanning();
        // The device pane is waiting on this too, so it shows progress rather
        // than an idle prompt.
        browser.set_device_loading();
        let notices = browser.scan_devices(&mut self.processes, None);
        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    pub(super) fn load_device_root(&mut self) {
        let port = self.devices.selected_port().map(str::to_string);
        // The first listing on a selected device must not be the thing that
        // silently kills a running script: when nothing is known about the
        // board's state, probe first and hold the listing until the probe
        // releases the port ([`super::probe`]). Checked before the browser
        // is taken below --- `start_device_probe` needs to see it.
        if self.start_device_probe(port.as_deref()) {
            if let Some(browser) = &mut self.browser {
                browser.set_device_loading();
            }
            return;
        }
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        let notices = browser.load_device(&mut self.processes, port.as_deref(), false);
        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
    }

    /// Updates what is believed about running user code on the selected
    /// device, and keeps the browser's interrupt gate in step: `Running`
    /// holds device requests until confirmed, anything else releases them
    /// (resuming whatever was held).
    ///
    /// Beliefs come from the probe, the monitor heuristic
    /// ([`Self::update_script_from_monitor`]), or ChipTUI's own actions ---
    /// an accepted interruption, a finished `run`, a `reset`. See
    /// [`ScriptState`] for what each value means and what it cannot know.
    pub(super) fn set_script_state(&mut self, state: ScriptState) {
        if self.devices.set_script_state(state) {
            match state {
                ScriptState::Running => self.logs.warn(
                    "device marked as running --- device operations will ask before interrupting",
                ),
                ScriptState::Stopped => {
                    self.logs.info("device script marked as stopped");
                }
                ScriptState::Unknown => {}
            }
        }
        if let Some(mut browser) = self.browser.take() {
            let port = self.devices.selected_port().map(str::to_string);
            browser.set_interrupt_gate(
                state == ScriptState::Running,
                &mut self.processes,
                port.as_deref(),
            );
            self.browser = Some(browser);
        }
    }

    /// Applies the monitor heuristic to what the live monitor session has
    /// shown so far. The monitor is passive --- it sends nothing unless the
    /// user types --- so this is the one signal that arrives *while* a script
    /// runs, rather than before or after touching the device.
    pub(super) fn update_script_from_monitor(&mut self) {
        let Some(state) = crate::device::monitor_script_activity(&self.device_monitor_output)
        else {
            return;
        };
        if self.devices.script_state() == state {
            return;
        }
        match state {
            ScriptState::Running => self.logs.info(
                "monitor shows a running script --- device operations will ask before interrupting",
            ),
            ScriptState::Stopped => self.logs.info("monitor shows an idle REPL"),
            ScriptState::Unknown => {}
        }
        self.set_script_state(state);
    }

    /// Opens the interrupt confirmation when device requests are being held.
    ///
    /// Polled after any key or process event that might have queued a
    /// request, so the overlay appears no matter which path armed the gate
    /// (a navigation key, a menu action, an automatic reload), and defers
    /// politely while another overlay is open.
    pub(super) fn check_interrupt_gate(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        if self
            .browser
            .as_ref()
            .is_some_and(Browser::held_for_interrupt)
        {
            self.overlay = Some(Overlay::ConfirmInterruptDevice { confirm: false });
        }
    }

    /// Asks how to bring an interrupted script back, once the operations the
    /// user accepted have drained ([`Overlay::RestoreDeviceScript`]).
    pub(super) fn maybe_offer_restore(&mut self) {
        if !self.restore_pending
            || self.overlay.is_some()
            || self.probe.is_some()
            || self.browser.as_ref().is_some_and(Browser::is_busy)
        {
            return;
        }
        self.restore_pending = false;
        self.overlay = Some(Overlay::RestoreDeviceScript {
            // "Leave it stopped" is the highlighted default: restarting
            // re-runs code the user may be mid-way through changing.
            selected: 2,
        });
    }

    pub(super) fn open_device_picker(&mut self) {
        self.overlay = Some(Overlay::DevicePicker {
            selected: self.devices.selected_index().unwrap_or(0),
        });
    }

    pub(super) fn apply_device_picker(&mut self, selected: usize) {
        if !self.devices.select(selected) {
            return;
        }
        let Some(device) = self.devices.selected() else {
            return;
        };
        self.logs.info(format!("device set to {}", device.label()));

        // Everything below is the MicroPython follow-through: an mpremote
        // probe and listing, then the esptool courtesy query. A backend
        // without a filesystem has neither tool, and must not have its port
        // touched by them --- for a Zephyr board the pick is the whole job
        // (the monitor uses the port on demand).
        if !self.manager.capabilities().contains(Capability::Filesystem) {
            return;
        }

        // A different board may be in a different state: probe it before the
        // first listing interrupts anything, same as the startup path.
        let port = self.devices.selected_port().map(str::to_string);
        if self.start_device_probe(port.as_deref()) {
            if let Some(browser) = &mut self.browser {
                browser.set_device_loading();
            }
            self.defer_device_info_query();
            return;
        }

        // The cached listing belongs to the previous board.
        match self.browser.take() {
            Some(mut browser) => {
                let notices = browser.load_device(&mut self.processes, port.as_deref(), true);
                self.browser = Some(browser);
                for (level, message) in notices {
                    self.logs.push(level, message);
                }
                // Same port-contention reasoning as the startup path: wait
                // for this listing to finish before esptool touches the port.
                self.defer_device_info_query();
            }
            // No filesystem capability, so nothing is about to contend for
            // the port --- safe to query right away.
            None => self.maybe_query_device_info(),
        }
    }

    pub(super) fn open_picker(&mut self) {
        let current = self.manager.override_kind();
        let selected = PickerOption::all()
            .iter()
            .position(|option| match (option, current) {
                (PickerOption::Automatic, None) => true,
                (PickerOption::Backend(kind), Some(active)) => *kind == active,
                _ => false,
            })
            .unwrap_or(0);
        self.overlay = Some(Overlay::BackendPicker { selected });
    }

    pub(super) fn apply_picker(&mut self, selected: usize) {
        let Some(option) = PickerOption::all().get(selected).copied() else {
            return;
        };
        match option {
            PickerOption::Automatic => {
                self.manager.set_override(None);
                match self.manager.selected_kind() {
                    Some(kind) => self
                        .logs
                        .info(format!("override cleared; detection selects {kind}")),
                    None => self
                        .logs
                        .warn("override cleared; detection did not identify a backend"),
                }
            }
            PickerOption::Backend(kind) => {
                self.manager.set_override(Some(kind));
                self.logs.info(format!("backend overridden to {kind}"));
                self.report_tools();
            }
        }
        // A switch away from a backend with no filesystem (e.g. Zephyr) never
        // created a browser to scan with; a switch onto one should not still
        // be sitting on "not scanned" just because it happened via the
        // picker instead of at startup.
        self.maybe_scan_devices();
        self.clamp_focus();
    }

    /// Applies the empty-project prompt's answer: an in-session override
    /// exactly like [`App::apply_picker`], plus persisting the choice to
    /// `chiptui.toml` so the directory needs no prompt on later runs.
    pub(super) fn apply_project_setup(&mut self, selected: usize) {
        let Some(kind) = BackendKind::ALL.get(selected).copied() else {
            return;
        };
        self.manager.set_override(Some(kind));
        match self.manager.write_scaffold(kind) {
            Ok(()) => self.logs.success(format!(
                "{kind} selected and saved to chiptui.toml in this project"
            )),
            Err(err) => self.logs.warn(format!(
                "{kind} selected for this session, but chiptui.toml could not be saved: {err}"
            )),
        }
        if kind == BackendKind::MicroPython
            && let Err(err) = self.manager.ensure_micropython_layout()
        {
            self.logs
                .warn(format!("could not create src/firmware layout: {err}"));
        }
        self.report_tools();
        self.maybe_scan_devices();
        self.clamp_focus();
    }

    /// Starts an interactive device monitor.
    pub fn open_monitor(&mut self) {
        let port = self.devices.selected_port().map(str::to_string);
        if let Some(command) = self
            .manager
            .backend()
            .and_then(|b| b.monitor_command(port.as_deref()))
            .map(|mut command| {
                // Same `[tools]` override seam the browser and run session
                // use, so tests (and user overrides) point every mpremote
                // invocation at the same binary. The browser owns the
                // mpremote override; the build panel owns west's.
                let tool = self
                    .browser
                    .as_ref()
                    .and_then(Browser::tool_path)
                    .or_else(|| self.build.as_ref().and_then(|panel| panel.tool_path()));
                if let Some(tool) = tool {
                    command = command.with_program(tool.to_string());
                }
                command
            })
        {
            // Otherwise the process starts receiving keystrokes only once the
            // user separately tabs over to the pane that just opened for it.
            self.focus = super::Focus::Logs;
            self.log_tab = LogTab::Monitor;
            self.monitor_source = MonitorSource::Device;
            self.device_monitor_output.clear();
            self.monitor_console.reset();

            // Spawn the monitor in a PTY so it stays inside the tab
            match self
                .processes
                .spawn_pty(command, std::time::Duration::from_secs(86400))
            {
                // 24h timeout
                Ok(id) => self.device_monitor_process = Some(id),
                Err(e) => {
                    self.logs.error(format!("could not open monitor: {}", e));
                    self.device_monitor_process = None;
                }
            }
        }
    }
}
