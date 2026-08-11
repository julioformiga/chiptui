//! Device paths and the minimal device manager.
//!
//! Only what the filesystem browser needs: enumerate serial ports, know which
//! one is selected, and address files on the device. Connection state lives in
//! `mpremote`, not here (`AGENTS.md` §2).

mod path;

pub use path::DevicePath;

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
}

impl DeviceState {
    pub fn new() -> Self {
        Self {
            known: Vec::new(),
            selected: None,
            discovery: DiscoveryState::Unknown,
            error: None,
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
            self.selected = Some(index);
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
        self.discovery = DiscoveryState::Ready;
        self.error = None;

        self.selected = previous
            .and_then(|port| self.known.iter().position(|device| device.port == port))
            .or(if self.known.len() == 1 { Some(0) } else { None });
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

    /// Description for the `device` slot in the header.
    pub fn summary(&self) -> String {
        match self.discovery {
            DiscoveryState::Unknown => "not scanned".to_string(),
            DiscoveryState::Scanning => "scanning…".to_string(),
            DiscoveryState::Failed => "unavailable".to_string(),
            DiscoveryState::Ready => match self.selected() {
                Some(device) => device.port.clone(),
                None if self.known.is_empty() => "none".to_string(),
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
        DeviceInfo {
            port: port.to_string(),
            serial: Some("e6614104".to_string()),
            vid_pid: "2e8a:0005".to_string(),
            description: "MicroPython Board".to_string(),
        }
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
}
