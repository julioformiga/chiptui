//! Device discovery and selection: scanning for a board, the device picker,
//! and applying a backend choice (manual override or the empty-project
//! prompt) --- the moments that can make a device or a filesystem-capable
//! backend newly available. Split out of `app.rs` since these are the
//! handful of places that decide *which* device/backend `App` is talking to,
//! as opposed to what to do once one is chosen.

use std::path::{Path, PathBuf};

use crate::backend::{BackendKind, Capability};
use crate::browser::Browser;

use super::{App, LogTab, MonitorSource, Overlay, PickerOption};

impl App {
    /// Scans for a connected device as soon as the project is known, without
    /// waiting for the user to move focus onto a file browser pane. A no-op
    /// once a browser already exists (`AGENTS.md` §5's "one `mpremote` at a
    /// time" applies just as much to not re-issuing a scan that already ran).
    ///
    /// Called from three places: `main.rs` right after startup, and
    /// [`App::apply_project_setup`]/[`App::apply_picker`] --- any moment the
    /// selected backend could newly gain [`Capability::Filesystem`].
    /// Deliberately **not** called from [`App::bootstrap`] itself, for the
    /// same reason [`App::maybe_open_project_setup`] is not: many existing
    /// tests call `bootstrap()` directly and do not expect a subprocess to
    /// spawn as a side effect.
    pub fn maybe_scan_devices(&mut self) {
        if !self.manager.capabilities().contains(Capability::Filesystem) {
            return;
        }
        self.ensure_browser_scanning();
    }

    /// Creates the browser and starts a device scan, if neither has already
    /// happened. Only the listing itself waits for the scan to name a port:
    /// issuing the scan now would let mpremote auto-connect to whichever
    /// board answers first, which is the guess `SPEC.md` §8 forbids.
    fn ensure_browser_scanning(&mut self) {
        if self.browser.is_some() {
            return;
        }
        let root = self
            .manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
        self.browser = Some(Browser::new(Self::initial_local_dir(root)));
        self.scan_devices();
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
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        let port = self.devices.selected_port().map(str::to_string);
        let notices = browser.load_device(&mut self.processes, port.as_deref(), false);
        self.browser = Some(browser);
        for (level, message) in notices {
            self.logs.push(level, message);
        }
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

        // The cached listing belongs to the previous board.
        match self.browser.take() {
            Some(mut browser) => {
                let port = self.devices.selected_port().map(str::to_string);
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
