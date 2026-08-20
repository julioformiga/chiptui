//! Device discovery and selection: scanning for a board, the device picker,
//! and applying a backend choice (manual override or the empty-project
//! prompt) --- the moments that can make a device or a filesystem-capable
//! backend newly available. Split out of `app.rs` since these are the
//! handful of places that decide *which* device/backend `App` is talking to,
//! as opposed to what to do once one is chosen.

use std::path::{Path, PathBuf};

use crate::backend::{BackendKind, Capability};
use crate::browser::Browser;
use crate::device::{DiscoveryState, ScriptState};
use crate::firmware_id::{FirmwareVerdict, FlashFirmware};

use super::flash_view::{FirmwareCheck, FirmwareHold};
use super::{App, DevicePaneTab, LogTab, MonitorSource, Overlay, PickerOption};

impl App {
    /// Ensures row 2's panes exist and the right scans start, without
    /// waiting for the user to move focus onto them: the browser for a
    /// backend that browses files (its device scan under
    /// [`Capability::Filesystem`]), the workspace+build pair for one that
    /// builds without a device filesystem. A no-op once the panes exist
    /// (`AGENTS.md` §5's "one `mpremote` at a time" applies just as much to
    /// not re-issuing a scan that already ran).
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
        self.ensure_mpy_projects();
        self.maybe_scan_serial_ports();
    }

    /// Places focus for the first frame. By now row 2's panes exist, so the
    /// tour can start on the workspace pane --- a build backend's
    /// environment questions come first --- instead of wherever the
    /// pre-panel clamps left the focus (`App::detect` runs before any pane
    /// exists, and clamps an unknown backend onto `Logs`). Startup-only:
    /// later backend switches keep the user's focus where it is, clamped
    /// to a pane that still exists.
    pub fn place_startup_focus(&mut self) {
        self.focus = self.fallback_pane();
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
        if self.devices.discovery != DiscoveryState::Unknown {
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

    /// Points workspace discovery's `~` at a directory other than `$HOME`
    /// --- how tests make the user-config and `~/zephyrproject` conventions
    /// deterministic. The config directory moves with it, so an inherited
    /// `$XDG_CONFIG_HOME` cannot pull the real user config back in behind
    /// the redirected home.
    pub fn set_home_dir(&mut self, dir: impl Into<std::path::PathBuf>) {
        self.home_dir = dir.into();
        self.config_dir = crate::settings::config_dir_in(&self.home_dir);
        // The project registry lives in that config, so redirecting the home
        // redirects which projects detection already knows about too.
        self.manager
            .set_known_projects(crate::settings::ProjectRegistry::load(
                &self.config_dir,
                &self.home_dir,
            ));
        // And so does `[ui] theme`, read once in `App::new` from the home
        // this call is replacing: everything the config answers has to move
        // with it, or a redirected home still renders the real user's
        // palette --- and their `theme = "auto"` still decides which row of
        // the theme picker reads `(active)`.
        self.theme = super::resolve_theme(&self.config_dir);
    }

    /// Scans `serial_dir` for USB serial ports and applies the result:
    /// one port selects itself, several ask, none reports why.
    pub fn scan_serial_devices(&mut self) {
        self.devices.set_scanning();
        let ports = crate::device::usb_serial_ports(&self.serial_dir);
        if ports.is_empty() {
            self.devices
                .set_failed("no USB serial port found — connect the board and press 'd'");
            // The departed board's identity does not outlive it --- same
            // rule as the mpremote scan's empty branch.
            self.device_disconnected();
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
        } else {
            // One port is not a guess: it selects itself, and the selection
            // carries the same follow-through as the picker --- here, the
            // chip identity query (this scan is the no-mpremote path, so
            // there is no listing to hold behind it).
            self.defer_device_info_query();
        }
    }

    /// Creates the browser (once), and starts a device scan with it when the
    /// backend has a filesystem to list. A backend that claims row 2 for
    /// workspace+build gets no browser at all: listing and editing the
    /// project's own files is the user's editor's job, not this tool's
    /// (`SPEC.md` §1 --- orchestration, not IDE). Only the listing itself
    /// waits for the scan to name a port: issuing the scan now would let
    /// mpremote auto-connect to whichever board answers first, which is the
    /// guess `SPEC.md` §8 forbids.
    fn ensure_browser_scanning(&mut self) {
        if self.browser.is_some() || self.build_pane_visible_precondition() {
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
    /// 2 (can build, no device filesystem), and --- for a backend that also
    /// maintains a workspace --- resolves the west environment first so the
    /// panel's commands carry it. Like the browser's creation this reads no
    /// subprocess: resolution reads two config files and walks the
    /// configured directory, and the build panel reads only the project's
    /// own `CMakeCache.txt` plus its registry entry's saved board/shield,
    /// if either exists.
    fn ensure_build_panel(&mut self) {
        if !self.build_pane_visible_precondition() || self.build.is_some() {
            return;
        }
        self.ensure_workspace_panel();
        let root = self
            .manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
        let mut panel = crate::build::BuildPanel::new(root.clone(), self.logs.offset());
        // The registry entry's board/shield answers --- the pickers'
        // persisted half --- outrank the build cache on every open, so a
        // project comes back with the target it was last built for. Still
        // nothing inside the project directory: the registry is the only
        // place a session answer is written to (`SPEC.md` §13).
        if let Some(entry) = self.manager.known_projects().entry_for(&root) {
            if let Some(board) = &entry.board {
                panel.set_config_board(board.clone());
            }
            if let Some(shield) = &entry.shield {
                panel.set_shield(Some(shield.clone()));
            }
        }
        self.build = Some(panel);
        // One place seeds a panel from the resolved environment --- the
        // west invocation *and* which of the two environment rows the
        // action stack offers. Duplicating it here is how the second fact
        // got missed the first time.
        self.apply_west_env();
        // The workspace pane's embedded file list starts on the same root
        // the build panel just claimed --- the checklist's `Project path`
        // answer and "what files does this pane show" are the same fact.
        if let Some(workspace) = &mut self.workspace {
            workspace.set_files_root(root);
        }
    }

    /// Resolves the Zephyr installation for a [`Capability::WorkspaceSync`]
    /// backend, once per session, from configuration and nowhere else: the
    /// project's `chiptui.toml` first, then the user config. Neither names
    /// a location → `NotConfigured`, and startup asks with the directory
    /// picker (`maybe_open_workspace_picker`); a configured location that is
    /// not an installation → `Invalid`, with the install guide in the
    /// message. No directory conventions, no environment variables.
    ///
    /// Called ahead of `ensure_build_panel` (the panel's commands carry the
    /// answer) and ahead of `App::report_tools` (the tool report judges
    /// `west` against the workspace's venv, so the workspace must exist
    /// first). That second ordering is written out at each call site rather
    /// than hidden inside the report: this reads two config files and can
    /// log a resolution error, which is not what a caller asking for a tool
    /// warning expects to trigger.
    pub(super) fn ensure_workspace_panel(&mut self) {
        if !self
            .manager
            .capabilities()
            .contains(Capability::WorkspaceSync)
            || self.workspace.is_some()
        {
            return;
        }
        let resolution = self.resolve_workspace();
        if let crate::backend::zephyr::workspace::Resolution::Invalid(message) = &resolution {
            self.logs.error(message.clone());
        }
        let projects_resolution = self.resolve_projects();
        if let crate::backend::zephyr::projects::ProjectsResolution::Invalid(message) =
            &projects_resolution
        {
            self.logs.error(message.clone());
        }
        let path_env = std::env::var("PATH").unwrap_or_default();
        let mut panel = crate::workspace::WorkspacePanel::new(resolution, path_env);
        panel.apply_projects(projects_resolution);
        self.workspace = Some(panel);
    }

    /// Assembles both config levels and resolves the installation location
    /// against them.
    pub(super) fn resolve_workspace(&self) -> crate::backend::zephyr::workspace::Resolution {
        let (_root, project_settings, user_settings) = self.zephyr_settings();
        let input = crate::backend::zephyr::workspace::ResolveInput {
            project_settings: project_settings.as_ref(),
            user_settings: user_settings.as_ref(),
            home: &self.home_dir,
        };
        crate::backend::zephyr::workspace::resolve(&input)
    }

    /// The projects-folder resolution over the same config levels --- the
    /// environment's second question, resolved with the same input so both
    /// answers agree on what "configured" means.
    pub(super) fn resolve_projects(&self) -> crate::backend::zephyr::projects::ProjectsResolution {
        let (_root, project_settings, user_settings) = self.zephyr_settings();
        let input = crate::backend::zephyr::workspace::ResolveInput {
            project_settings: project_settings.as_ref(),
            user_settings: user_settings.as_ref(),
            home: &self.home_dir,
        };
        crate::backend::zephyr::projects::resolve(&input)
    }

    /// The project root plus both parsed `[zephyr]` sections (project file,
    /// then user config), the shared input of resolution and picker
    /// validation. Reading them together is what keeps one definition of
    /// "what is configured" across both doors.
    pub(super) fn zephyr_settings(
        &self,
    ) -> (
        PathBuf,
        Option<crate::settings::ZephyrSettings>,
        Option<crate::settings::ZephyrSettings>,
    ) {
        let root = self
            .manager
            .root()
            .map_or_else(|| self.manager.start_dir().to_path_buf(), Path::to_path_buf);
        let project_settings =
            std::fs::read_to_string(root.join(crate::project::config::FILE_NAME))
                .ok()
                .map(|text| crate::settings::ZephyrSettings::parse(&text))
                .filter(|settings| !settings.is_empty());
        let user_settings = crate::settings::load_user(&self.config_dir);
        (root, project_settings, user_settings)
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
        // Next in line is the board's identity, not its files: hold the
        // listing behind the background `esptool chip-id` until that query
        // finishes ([`super::flash_view`]). Backends without the query
        // (no esptool-backed capability, i.e. no first-listing flow at all
        // for a build-only backend like Zephyr) never hold here.
        if self.hold_root_listing_for_chip_identity() {
            if let Some(browser) = &mut self.browser {
                browser.set_device_loading();
            }
            return;
        }
        // Then the firmware that identity read opens the question of: only
        // MicroPython exposes a filesystem mpremote can walk, so the
        // listing waits for the identification read's verdict and is
        // refused --- with the reason in the pane --- when the board runs
        // something else (`super::flash_view`).
        match self.hold_root_listing_for_firmware() {
            FirmwareHold::Release => {}
            FirmwareHold::Held => {
                if let Some(browser) = &mut self.browser {
                    browser.set_device_loading();
                }
                return;
            }
            FirmwareHold::Blocked => return,
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
    /// runs, rather than before or after touching the device. The banner it
    /// inevitably prints also refreshes the board's MicroPython version.
    pub(super) fn update_script_from_monitor(&mut self) {
        // Computed before the mutable use below: the lines are borrowed
        // immutably here and `self` mutably by the assignment.
        if let Some(version) = crate::device::micropython_version(&self.device_monitor_output) {
            self.mpy_version = Some(version);
        }
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
    /// Two things can be waiting on the user's say-so: browser requests
    /// queued behind the interrupt gate (a navigation key, a menu action),
    /// and the identification chain itself --- when the probe believes a
    /// script is running, the chip query politely waits for the belief to
    /// clear and the first listing is held behind that query, so without
    /// asking nothing would ever move. The same question covers both
    /// (esptool resets the board to read the chip, which interrupts
    /// exactly like mpremote's raw REPL does).
    ///
    /// Polled after any key or process event that might have queued a
    /// request, and on every tick, so the overlay appears no matter which
    /// path armed the gate, and defers politely while another overlay is
    /// open.
    pub(super) fn check_interrupt_gate(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        let browser_gated = self
            .browser
            .as_ref()
            .is_some_and(Browser::held_for_interrupt);
        let chain_gated = self.held_root_listing
            && self.flash_query_pending
            && self.devices.script_state() == ScriptState::Running;
        if browser_gated || chain_gated {
            self.overlay = Some(Overlay::ConfirmInterruptDevice { confirm: false });
        }
    }

    /// Declines the interrupt on the identification chain's half: the held
    /// listing is dropped with the same explainable state the browser's
    /// held requests get, so the pane does not spin on "loading" forever
    /// (`Overlay::ConfirmInterruptDevice`'s decline path).
    pub(super) fn decline_held_listing(&mut self) {
        if !self.held_root_listing {
            return;
        }
        self.held_root_listing = false;
        self.set_device_pane_error("cancelled — a script is running on the device");
    }

    /// Asks how to bring an interrupted script back, once the operations the
    /// user accepted have drained ([`Overlay::RestoreDeviceScript`]).
    /// "Drained" includes the identification chain: an accepted
    /// interruption moves the chip query and the firmware read first, and
    /// the restore question must not open on top of the port they hold.
    pub(super) fn maybe_offer_restore(&mut self) {
        if !self.restore_pending
            || self.overlay.is_some()
            || self.probe.is_some()
            || self.browser.as_ref().is_some_and(Browser::is_busy)
            || self.flash_query_pending
            || self.firmware_check != FirmwareCheck::Idle
            || self
                .flash
                .as_ref()
                .is_some_and(crate::flash::FlashPanel::is_busy)
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
        // The REPL-banner version belonged to whichever board was selected
        // before; the new board re-answers it through its own probe/monitor.
        self.mpy_version = None;
        // So did the firmware verdict: the new board's chip query re-arms
        // the identification, and until then `undefined` is the honest
        // display --- not the previous board's answer.
        self.firmware_check_port = None;
        self.firmware_check = FirmwareCheck::Idle;
        if let Some(flash) = &mut self.flash {
            flash.clear_firmware_identity();
        }

        // Everything below is the MicroPython follow-through: an mpremote
        // probe and listing. A backend without a filesystem has neither
        // tool, and must not have its port touched by them --- for a Zephyr
        // board no listing happens (the monitor uses the port on demand).
        // The board's *identity* is still worth asking, though: Zephyr runs
        // on ESP32 boards whose esptool runner answers `chip-id`, and on
        // anything else the query fails harmlessly into the pane's honest
        // placeholder.
        if !self.manager.capabilities().contains(Capability::Filesystem) {
            self.defer_device_info_query();
            return;
        }

        // A different board may be in a different state: probe it before the
        // first listing interrupts anything, same as the startup path.
        let port = self.devices.selected_port().map(str::to_string);
        if let Some(browser) = &mut self.browser {
            // The cached listing belongs to the previous board; drop it
            // before the probe/chip query/listing chain below can show it
            // under the new selection.
            browser.invalidate_device_cache();
        }
        if self.start_device_probe(port.as_deref()) {
            if let Some(browser) = &mut self.browser {
                browser.set_device_loading();
            }
            self.defer_device_info_query();
            return;
        }

        match self.browser.as_mut() {
            // Same port-contention reasoning as the startup path: the chip
            // query goes first and the listing queues behind it.
            Some(_) => {
                self.defer_device_info_query();
                self.load_device_root();
            }
            // No filesystem capability, so nothing is about to contend for
            // the port --- safe to query right away.
            None => {
                self.maybe_query_device_info();
            }
        }
    }

    /// `r` on the device pane: re-list, through the firmware gate. With
    /// MicroPython confirmed for this port it is the plain forced reload
    /// the key has always been; without a confirmation it re-runs the
    /// identification first --- the recovery path after flashing
    /// MicroPython over a different firmware, where the pane sat refused
    /// until the board's answer changed.
    pub(super) fn reload_device_pane(&mut self) {
        let port = self.devices.selected_port().map(str::to_string);
        let confirmed = port.as_deref().is_some_and(|port| {
            self.firmware_check_port.as_deref() == Some(port)
                && self.flash.as_ref().is_some_and(|flash| {
                    matches!(
                        flash.details.firmware,
                        Some(FirmwareVerdict::Firmware(FlashFirmware::MicroPython, _))
                    )
                })
        });
        if !confirmed {
            // Re-arm the identification for this port the same way the chip
            // query's finish does: the pending read and the port it belongs
            // to are one fact (`arm_firmware_check`), or the gate would see
            // an unclaimed port and list straight away.
            self.firmware_check_port = port;
            self.firmware_check = if self
                .flash
                .as_ref()
                .is_some_and(|flash| flash.details.family.is_some())
            {
                // Only ask again when esptool has ever read this board's
                // chip: a board whose chip query cannot succeed (no
                // esptool-backed bootloader) would just pay for a refused
                // read on every reload.
                FirmwareCheck::Pending
            } else {
                FirmwareCheck::Idle
            };
            self.load_device_root();
            return;
        }
        self.dispatch_browser(|browser, processes, port| {
            browser.load_device(processes, port, true)
        });
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
                self.ensure_workspace_panel();
                self.report_tools();
            }
        }
        // The pane the old backend showed is not this backend's pane: the
        // actions tab belongs to a device pane that may not even exist for
        // the one being switched to, so the tab starts over with it.
        self.device_pane_tab = DevicePaneTab::Files;
        // A switch away from a backend with no filesystem (e.g. Zephyr) never
        // created a browser to scan with; a switch onto one should not still
        // be sitting on "not scanned" just because it happened via the
        // picker instead of at startup.
        self.maybe_scan_devices();
        self.clamp_focus();
    }

    /// Applies the empty-project prompt's answer (`SPEC.md` §7): an
    /// in-session override exactly like [`App::apply_picker`], the backend's
    /// own starting layout written into the directory, and the answer itself
    /// recorded in the user config so the directory needs no prompt on later
    /// runs --- and so the home screen lists it.
    pub(super) fn apply_project_setup(&mut self, selected: usize) {
        let Some(kind) = BackendKind::ALL.get(selected).copied() else {
            return;
        };
        self.manager.set_override(Some(kind));
        // The pane the old backend showed is not this backend's pane: the
        // actions tab belongs to the device pane that may not even exist
        // yet, so the tab starts over with it.
        self.device_pane_tab = DevicePaneTab::Files;
        match self.manager.create_scaffold(kind) {
            Ok(created) if created.written.is_empty() => {
                self.logs.success(format!("{kind} selected"));
            }
            Ok(created) => {
                let names: Vec<String> = created
                    .written
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                self.logs
                    .success(format!("{kind} selected --- created {}", names.join(", ")));
            }
            Err(err) => self.logs.warn(format!(
                "{kind} selected, but the project layout could not be created: {err}"
            )),
        }
        self.record_open_project();
        self.ensure_workspace_panel();
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
            self.set_monitor_source(MonitorSource::Device);
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
