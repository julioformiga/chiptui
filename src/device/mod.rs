//! Device paths and the minimal device manager.
//!
//! Only what the filesystem browser needs: enumerate serial ports, know which
//! one is selected, and address files on the device. Connection state lives in
//! `mpremote`, not here (`AGENTS.md` §2).

mod path;
mod vendor;

pub use path::DevicePath;

/// USB serial port paths under `dir`, sorted --- the port-discovery half of
/// device scanning for a backend that has no listing tool of its own
/// (`mpremote devs` does this job for MicroPython; Zephyr's `west monitor`
/// just wants a `/dev/ttyACM*` handed to it), and the count hotplug
/// detection compares against `last_port_count` (same prefix set, so a
/// change in what it counts is exactly a change worth rescanning).
/// CDC-ACM and USB-serial on Linux, `cu.usb`/`tty.usb` on macOS. Legacy
/// `ttyS*` UARTs are deliberately excluded --- a typical machine has
/// dozens, none of them a development board.
pub fn usb_serial_ports(dir: &std::path::Path) -> Vec<String> {
    #[cfg(unix)]
    {
        let mut ports: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_usb = name.starts_with("ttyUSB")
                    || name.starts_with("ttyACM")
                    || name.starts_with("cu.usb")
                    || name.starts_with("tty.usb");
                is_usb.then(|| dir.join(&name).display().to_string())
            })
            .collect();
        ports.sort();
        ports
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Vec::new()
    }
}

/// A serial device as reported by `mpremote devs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Port path, e.g. `/dev/ttyACM0`. This is what is passed to `connect`.
    pub port: String,
    /// USB serial number, or `None` when the port does not report one.
    pub serial: Option<String>,
    /// `vid:pid`, as printed.
    pub vid_pid: String,
    /// Manufacturer and product, joined as printed.
    pub description: String,
}

impl DeviceInfo {
    /// Short label for the header and the picker.
    pub fn label(&self) -> String {
        if self.description.is_empty() {
            self.port.clone()
        } else {
            format!("{} ({})", self.port, self.description)
        }
    }

    /// A recognisable vendor name for this `vid:pid`, if any.
    ///
    /// Labeling only, never filtering: an unrecognised USB serial device is
    /// still a legitimate candidate (`SPEC.md` §8), it simply carries no
    /// extra hint.
    pub fn vendor(&self) -> Option<&'static str> {
        vendor::label_for(&self.vid_pid)
    }

    /// The micropython.org/download/ `vendor=` filter value for this device,
    /// if its vid:pid identifies an actual board vendor rather than a
    /// generic USB-serial bridge chip (`SPEC.md` §9).
    pub fn board_vendor(&self) -> Option<&'static str> {
        vendor::board_vendor_for(&self.vid_pid)
    }
}

/// Whether user code is believed to be running on the selected device.
///
/// `mpremote` interrupts whatever is running (Ctrl-C, then raw REPL) for
/// *every* filesystem or `exec` command, so this is what decides whether a
/// device operation needs the user's explicit go-ahead first. It is a belief,
/// not a fact: the serial port offers no non-invasive way to ask the board,
/// so the state comes from what a passive observer can see (see
/// [`monitor_script_activity`]) and from ChipTUI's own interruptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptState {
    /// Nothing observed yet --- operations proceed without asking, which is
    /// the pre-probe behavior (and the documented limitation for a script
    /// that never prints: silence is indistinguishable from an idle board).
    #[default]
    Unknown,
    /// User code is believed to be executing. Device operations are held
    /// until the user confirms the interruption.
    Running,
    /// Known idle: an interrupted script not yet restarted, or a visible
    /// REPL prompt.
    Stopped,
}

/// What a stream of rendered REPL/monitor lines says about running user code.
///
/// A REPL prompt (`>>> `) means user code is *not* running; a board that only
/// ever prints output, with no prompt, is running a script. Both facts are
/// exactly what a human watching the monitor would conclude --- the rule just
/// automates that judgment so the file browser can ask before interrupting.
///
/// Banner lines (mpremote's own, MicroPython's version header and its help
/// hint) are ignored, so an idle board that has only shown its banner does not
/// count as "running" while its prompt is still on the way. `None` means
/// "not enough evidence either way": a silent script looks identical to a
/// silent board, and pretending otherwise would interrupt on a guess.
pub fn monitor_script_activity(lines: &[String]) -> Option<ScriptState> {
    const PROMPT: &str = ">>> ";
    /// Output lines that carry no signal about who produced them.
    const MIN_LINES: usize = 3;

    if lines.iter().any(|line| line.contains(PROMPT)) {
        return Some(ScriptState::Stopped);
    }
    let meaningful = lines
        .iter()
        .filter(|line| !is_banner(line) && !line.trim().is_empty())
        .count();
    (meaningful >= MIN_LINES).then_some(ScriptState::Running)
}

/// Lines either tool prints on connect, before any board output arrives.
fn is_banner(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("Connected to MicroPython")
        || line.starts_with("Type Ctrl-")
        || line.starts_with("Use Ctrl-")
        // MicroPython's own header: `MicroPython v1.28.0 on 2025-...; board`.
        || (line.starts_with("MicroPython v") && line.contains(" on "))
        || line.starts_with("Type \"help()\" for more information")
}

/// What the device manager currently knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryState {
    /// `mpremote devs` has not been run yet.
    Unknown,
    Scanning,
    Ready,
    Failed,
}

/// Known devices and the current selection.
#[derive(Debug, Clone)]
pub struct DeviceState {
    known: Vec<DeviceInfo>,
    selected: Option<usize>,
    pub discovery: DiscoveryState,
    /// Why discovery failed, if it did.
    pub error: Option<String>,
    /// Whether user code is believed to be running on the selected device
    /// (`ScriptState`'s doc comment explains where the belief comes from).
    script: ScriptState,
}

impl DeviceState {
    pub fn new() -> Self {
        Self {
            known: Vec::new(),
            selected: None,
            discovery: DiscoveryState::Unknown,
            error: None,
            script: ScriptState::Unknown,
        }
    }

    pub fn devices(&self) -> &[DeviceInfo] {
        &self.known
    }

    pub fn selected(&self) -> Option<&DeviceInfo> {
        self.selected.and_then(|index| self.known.get(index))
    }

    /// Port to pass to `mpremote connect`, if a device is selected.
    pub fn selected_port(&self) -> Option<&str> {
        self.selected().map(|device| device.port.as_str())
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index < self.known.len() {
            let previous = self.selected_port().map(str::to_string);
            self.selected = Some(index);
            // A different board may well be in a different state; what was
            // true of the old port says nothing about the new one.
            if previous.as_deref() != self.selected_port() {
                self.script = ScriptState::Unknown;
            }
            true
        } else {
            false
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// Records a completed scan.
    ///
    /// A single device selects itself: asking the user to choose from a list of
    /// one is noise. With several, the selection is left empty so the caller can
    /// prompt --- never guessing which board the user meant (`SPEC.md` §8).
    /// An existing selection is preserved if that port is still present.
    pub fn set_devices(&mut self, devices: Vec<DeviceInfo>) {
        let previous = self.selected().map(|device| device.port.clone());
        self.known = devices;
        // Stable: ties (including "both unknown") keep mpremote's original
        // order, so a recognised board surfaces first without reshuffling
        // devices the vendor table has nothing to say about.
        self.known.sort_by_key(|device| device.vendor().is_none());
        self.discovery = DiscoveryState::Ready;
        self.error = None;

        self.selected = previous
            .and_then(|port| self.known.iter().position(|device| device.port == port))
            .or(if self.known.len() == 1 { Some(0) } else { None });
        // A hotplug swap can land a different board on the very same port;
        // whatever was known about the previous one is void.
        self.script = ScriptState::Unknown;
    }

    pub fn set_scanning(&mut self) {
        self.discovery = DiscoveryState::Scanning;
        self.error = None;
    }

    pub fn set_failed(&mut self, error: impl Into<String>) {
        self.discovery = DiscoveryState::Failed;
        self.error = Some(error.into());
        self.known.clear();
        self.selected = None;
    }

    /// Whether a scan found several devices but none was chosen.
    pub fn needs_selection(&self) -> bool {
        self.discovery == DiscoveryState::Ready && self.known.len() > 1 && self.selected.is_none()
    }

    /// The believed script state of the selected device.
    pub fn script_state(&self) -> ScriptState {
        self.script
    }

    /// Updates the believed script state, returning whether it changed.
    pub fn set_script_state(&mut self, state: ScriptState) -> bool {
        let changed = self.script != state;
        self.script = state;
        changed
    }

    /// Description for the `device` slot in the header.
    pub fn summary(&self) -> String {
        match self.discovery {
            DiscoveryState::Unknown => "not scanned".to_string(),
            DiscoveryState::Scanning => "scanning…".to_string(),
            DiscoveryState::Failed => "unavailable".to_string(),
            DiscoveryState::Ready => match self.selected() {
                Some(device) => {
                    let mut label = match device.vendor() {
                        Some(vendor) => format!("{} ({vendor})", device.port),
                        None => device.port.clone(),
                    };
                    if self.script == ScriptState::Running {
                        label.push_str(" (script running)");
                    }
                    label
                }
                None if self.known.is_empty() => "none".to_string(),
                None => format!("{} found, none selected", self.known.len()),
            },
        }
    }
    /// Compact status for the header's right edge: the port when a device
    /// answers, otherwise the reason none does. Vendor and script suffixes
    /// stay out --- one line has no room for them, and the picker and the
    /// Monitor tab already tell that story.
    pub fn header_status(&self) -> String {
        match self.discovery {
            DiscoveryState::Unknown => "not scanned".to_string(),
            DiscoveryState::Scanning => "scanning…".to_string(),
            DiscoveryState::Failed => "unavailable".to_string(),
            DiscoveryState::Ready => match self.selected() {
                Some(device) => device.port.clone(),
                None if self.known.is_empty() => "no device".to_string(),
                None => format!("{} found, none selected", self.known.len()),
            },
        }
    }
}

impl Default for DeviceState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(port: &str) -> DeviceInfo {
        device_with_vid(port, "2e8a:0005")
    }

    fn device_with_vid(port: &str, vid_pid: &str) -> DeviceInfo {
        DeviceInfo {
            port: port.to_string(),
            serial: Some("e6614104".to_string()),
            vid_pid: vid_pid.to_string(),
            description: "MicroPython Board".to_string(),
        }
    }

    #[test]
    fn usb_serial_ports_keep_boards_and_drop_legacy_uarts() {
        let dir = std::env::temp_dir().join(format!("chiptui-ports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "ttyS0", "ttyS31", "ttyACM1", "ttyACM0", "ttyUSB3", "ttyUSB0", "null", "console",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }

        let ports = usb_serial_ports(&dir);
        assert_eq!(
            ports,
            vec![
                dir.join("ttyACM0").display().to_string(),
                dir.join("ttyACM1").display().to_string(),
                dir.join("ttyUSB0").display().to_string(),
                dir.join("ttyUSB3").display().to_string(),
            ],
            "only USB serial ports, sorted --- the dozens of legacy ttyS* \
             UARTs on a typical machine are never boards"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_dev_directory_is_no_ports_not_an_error() {
        assert!(usb_serial_ports(std::path::Path::new("/nonexistent-dev")).is_empty());
    }

    #[test]
    fn a_single_device_selects_itself() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0")]);

        assert_eq!(state.selected_port(), Some("/dev/ttyACM0"));
        assert!(!state.needs_selection());
    }

    #[test]
    fn several_devices_require_an_explicit_choice() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0"), device("/dev/ttyUSB0")]);

        assert_eq!(state.selected_port(), None, "must not guess which board");
        assert!(state.needs_selection());

        assert!(state.select(1));
        assert_eq!(state.selected_port(), Some("/dev/ttyUSB0"));
        assert!(!state.needs_selection());
    }

    #[test]
    fn rescanning_keeps_the_selected_port() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0"), device("/dev/ttyUSB0")]);
        state.select(1);

        // Same devices, different order.
        state.set_devices(vec![device("/dev/ttyUSB0"), device("/dev/ttyACM0")]);
        assert_eq!(state.selected_port(), Some("/dev/ttyUSB0"));
    }

    #[test]
    fn a_selection_that_disappears_is_dropped() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0"), device("/dev/ttyUSB0")]);
        state.select(0);

        state.set_devices(vec![device("/dev/ttyUSB0")]);
        // Only one left, so it takes over rather than leaving a dangling port.
        assert_eq!(state.selected_port(), Some("/dev/ttyUSB0"));
    }

    #[test]
    fn out_of_range_selection_is_rejected() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0")]);
        assert!(!state.select(7));
        assert_eq!(state.selected_port(), Some("/dev/ttyACM0"));
    }

    #[test]
    fn failure_clears_devices_and_is_reported() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0")]);
        state.set_failed("mpremote not found");

        assert_eq!(state.selected_port(), None);
        assert!(state.devices().is_empty());
        assert_eq!(state.summary(), "unavailable");
    }

    #[test]
    fn known_vendors_sort_before_unknown_ones() {
        let mut state = DeviceState::new();
        state.set_devices(vec![
            device_with_vid("/dev/ttyUSB0", "1234:5678"),
            device_with_vid("/dev/ttyACM0", "2e8a:0005"),
        ]);

        assert_eq!(
            state.devices()[0].port,
            "/dev/ttyACM0",
            "a recognised board sorts first"
        );
        assert_eq!(state.devices()[1].port, "/dev/ttyUSB0");
    }

    #[test]
    fn devices_with_the_same_recognition_keep_their_scan_order() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0"), device("/dev/ttyUSB0")]);

        assert_eq!(state.devices()[0].port, "/dev/ttyACM0");
        assert_eq!(state.devices()[1].port, "/dev/ttyUSB0");
    }

    #[test]
    fn board_vendor_is_narrower_than_the_display_label() {
        let espressif = device_with_vid("/dev/ttyACM0", "303a:1001");
        assert_eq!(espressif.board_vendor(), Some("Espressif"));

        // CP210x is a bridge chip used by many unrelated boards: it gets a
        // display label but no board-vendor filter value.
        let bridge = device_with_vid("/dev/ttyUSB0", "10c4:ea60");
        assert!(bridge.vendor().is_some());
        assert_eq!(bridge.board_vendor(), None);
    }

    #[test]
    fn summary_includes_a_known_vendor_label() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0")]);
        assert_eq!(state.summary(), "/dev/ttyACM0 (Raspberry Pi (RP2040))");
    }

    #[test]
    fn summary_reflects_each_discovery_state() {
        let mut state = DeviceState::new();
        assert_eq!(state.summary(), "not scanned");
        state.set_scanning();
        assert_eq!(state.summary(), "scanning…");
        state.set_devices(Vec::new());
        assert_eq!(state.summary(), "none");
        state.set_devices(vec![device("/dev/ttyACM0"), device("/dev/ttyUSB0")]);
        assert_eq!(state.summary(), "2 found, none selected");
    }

    #[test]
    fn header_status_names_the_port_without_vendor_or_script() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0")]);
        state.set_script_state(ScriptState::Running);
        assert_eq!(state.header_status(), "/dev/ttyACM0");
    }

    #[test]
    fn header_status_reflects_each_discovery_state() {
        let mut state = DeviceState::new();
        assert_eq!(state.header_status(), "not scanned");
        state.set_scanning();
        assert_eq!(state.header_status(), "scanning…");
        state.set_failed("mpremote not found");
        assert_eq!(state.header_status(), "unavailable");
        state.set_devices(Vec::new());
        assert_eq!(state.header_status(), "no device");
        state.set_devices(vec![device("/dev/ttyACM0"), device("/dev/ttyUSB0")]);
        assert_eq!(state.header_status(), "2 found, none selected");
    }

    #[test]
    fn a_running_script_is_announced_in_the_summary() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0")]);
        assert_eq!(state.summary(), "/dev/ttyACM0 (Raspberry Pi (RP2040))");

        assert!(state.set_script_state(ScriptState::Running));
        assert_eq!(
            state.summary(),
            "/dev/ttyACM0 (Raspberry Pi (RP2040)) (script running)"
        );
        assert!(
            !state.set_script_state(ScriptState::Running),
            "no repeat log"
        );
    }

    #[test]
    fn switching_ports_forgets_the_script_state() {
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0"), device("/dev/ttyUSB0")]);
        state.select(0);
        state.set_script_state(ScriptState::Running);

        state.select(1);
        assert_eq!(state.script_state(), ScriptState::Unknown);
    }

    #[test]
    fn a_rescan_forgets_the_script_state() {
        // A hotplug swap can land a different board on the same port.
        let mut state = DeviceState::new();
        state.set_devices(vec![device("/dev/ttyACM0")]);
        state.set_script_state(ScriptState::Running);

        state.set_devices(vec![device("/dev/ttyACM0")]);
        assert_eq!(state.script_state(), ScriptState::Unknown);
    }

    #[test]
    fn monitor_output_with_a_prompt_means_idle() {
        let lines: Vec<String> = vec![
            "MicroPython v1.28.0 on 2026-08-14; ESP32 board with ESP32S3".into(),
            "Type \"help()\" for more information.".into(),
            ">>> ".into(),
        ];
        assert_eq!(monitor_script_activity(&lines), Some(ScriptState::Stopped));
    }

    #[test]
    fn monitor_output_without_a_prompt_means_a_running_script() {
        let lines: Vec<String> = vec![
            "temp: 21.3".into(),
            "temp: 21.4".into(),
            "temp: 21.4".into(),
        ];
        assert_eq!(monitor_script_activity(&lines), Some(ScriptState::Running));
    }

    #[test]
    fn a_banner_alone_is_not_evidence_of_a_script() {
        // Two banner lines is what an idle board shows before its prompt
        // arrives; counting them would flag every connect as "running".
        let lines: Vec<String> = vec![
            "MicroPython v1.28.0 on 2026-08-14; ESP32 board".into(),
            "Type \"help()\" for more information.".into(),
        ];
        assert_eq!(monitor_script_activity(&lines), None);
    }

    #[test]
    fn silence_is_not_evidence_either() {
        assert_eq!(monitor_script_activity(&[]), None);
        assert_eq!(
            monitor_script_activity(&["".to_string(), "   ".to_string()]),
            None
        );
    }

    #[test]
    fn a_prompt_outranks_accumulated_output() {
        // A script that ended after printing leaves the REPL prompt behind;
        // the last word belongs to the prompt.
        let lines: Vec<String> = vec![
            "working...".into(),
            "working...".into(),
            "done".into(),
            ">>> ".into(),
        ];
        assert_eq!(monitor_script_activity(&lines), Some(ScriptState::Stopped));
    }
}
