//! Project manager: root discovery, backend detection and manual override.

mod detect;
mod scan;

use std::path::{Path, PathBuf};

use crate::backend::{Backend, BackendKind, BackendRegistry, Capabilities};
use crate::error::Result;

pub use detect::{
    AMBIGUITY_MARGIN, AUTO_CONFIDENCE, BackendScore, Detection, DetectionOutcome, DetectionSource,
    MAX_SEARCH_DEPTH, MIN_CONFIDENCE, Signal, classify, detect_from, score_directory,
};
pub use scan::{DirScan, TEXT_FILES};

/// Owns the current project and the backends it could belong to.
///
/// Detection is explicit: nothing re-detects behind the user's back, so the
/// displayed evidence always matches the displayed conclusion.
pub struct ProjectManager {
    registry: BackendRegistry,
    /// Directory the search starts from --- usually the process's cwd.
    start_dir: PathBuf,
    /// User's choice, which survives re-detection until cleared.
    override_kind: Option<BackendKind>,
    detection: Option<Detection>,
}

impl ProjectManager {
    pub fn new(start_dir: impl Into<PathBuf>) -> Self {
        Self {
            registry: BackendRegistry::with_builtin_backends(),
            start_dir: start_dir.into(),
            override_kind: None,
            detection: None,
        }
    }

    pub fn registry(&self) -> &BackendRegistry {
        &self.registry
    }

    pub fn start_dir(&self) -> &Path {
        &self.start_dir
    }

    pub fn detection(&self) -> Option<&Detection> {
        self.detection.as_ref()
    }

    pub fn override_kind(&self) -> Option<BackendKind> {
        self.override_kind
    }

    /// Runs detection from [`ProjectManager::start_dir`], re-applying any
    /// active override.
    pub fn detect(&mut self) -> Result<&Detection> {
        let mut detection = detect_from(&self.registry, &self.start_dir)?;
        if let Some(kind) = self.override_kind {
            detection = detection.overridden_with(kind);
        }
        Ok(self.detection.insert(detection))
    }

    /// Sets or clears the manual backend override (`AGENTS.md` §4).
    ///
    /// Clearing restores the automatic conclusion from the evidence already
    /// gathered --- no filesystem access needed.
    pub fn set_override(&mut self, kind: Option<BackendKind>) {
        self.override_kind = kind;
        let Some(detection) = self.detection.take() else {
            return;
        };
        self.detection = Some(match kind {
            Some(kind) => detection.overridden_with(kind),
            None => Detection {
                outcome: classify(&detection.scores),
                source: DetectionSource::Automatic,
                ..detection
            },
        });
    }

    /// The project root, once detection has run.
    pub fn root(&self) -> Option<&Path> {
        self.detection.as_ref().map(|d| d.root.as_path())
    }

    /// Directory name of the project root, for display.
    pub fn name(&self) -> Option<&str> {
        self.root()
            .and_then(Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
    }

    pub fn selected_kind(&self) -> Option<BackendKind> {
        self.detection.as_ref().and_then(Detection::backend)
    }

    pub fn backend(&self) -> Option<&dyn Backend> {
        self.selected_kind()
            .and_then(|kind| self.registry.get(kind))
    }

    /// Capabilities of the selected backend --- empty when none is selected.
    ///
    /// This is what the UI renders actions from; it never asks which framework
    /// is in play (`SPEC.md` §4.3).
    pub fn capabilities(&self) -> Capabilities {
        self.registry.capabilities(self.selected_kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Capability;

    /// Detection that would have concluded MicroPython on its own.
    fn manager_with_detection() -> ProjectManager {
        let mut manager = ProjectManager::new("/p");
        let scan = DirScan::from_parts("/p", ["boot.py", "main.py"], [], []);
        let scores = score_directory(&manager.registry, &scan);
        manager.detection = Some(Detection {
            root: PathBuf::from("/p"),
            outcome: classify(&scores),
            scores,
            source: DetectionSource::Automatic,
            searched: vec![PathBuf::from("/p")],
        });
        manager
    }

    #[test]
    fn capabilities_follow_the_selected_backend() {
        let manager = manager_with_detection();
        assert_eq!(manager.selected_kind(), Some(BackendKind::MicroPython));
        assert!(manager.capabilities().contains(Capability::Repl));
        assert!(!manager.capabilities().contains(Capability::Build));
    }

    #[test]
    fn override_switches_capabilities_without_new_detection() {
        let mut manager = manager_with_detection();
        manager.set_override(Some(BackendKind::Zephyr));

        assert_eq!(manager.selected_kind(), Some(BackendKind::Zephyr));
        assert!(manager.capabilities().contains(Capability::Build));
        assert!(!manager.capabilities().contains(Capability::Repl));
        assert_eq!(manager.detection().unwrap().source, DetectionSource::Manual);
    }

    #[test]
    fn clearing_the_override_restores_the_automatic_conclusion() {
        let mut manager = manager_with_detection();
        manager.set_override(Some(BackendKind::Zephyr));
        manager.set_override(None);

        assert_eq!(manager.selected_kind(), Some(BackendKind::MicroPython));
        assert_eq!(
            manager.detection().unwrap().source,
            DetectionSource::Automatic
        );
        assert_eq!(manager.override_kind(), None);
    }

    #[test]
    fn no_detection_means_no_capabilities() {
        let manager = ProjectManager::new("/p");
        assert!(manager.capabilities().is_empty());
        assert_eq!(manager.selected_kind(), None);
        assert!(manager.backend().is_none());
        assert!(manager.name().is_none());
    }

    #[test]
    fn setting_an_override_before_detection_is_applied_on_the_next_run() {
        let mut manager = ProjectManager::new("/p");
        manager.set_override(Some(BackendKind::Zephyr));
        assert_eq!(manager.override_kind(), Some(BackendKind::Zephyr));
        assert_eq!(manager.selected_kind(), None, "nothing detected yet");
    }
}
