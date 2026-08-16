//! Project manager: root discovery, backend detection and manual override.

pub mod config;
mod detect;
pub mod scaffold;
mod scan;

use std::io;
use std::path::{Path, PathBuf};

use crate::backend::{Backend, BackendKind, BackendRegistry, Capabilities};
use crate::error::Result;

pub use detect::{
    AMBIGUITY_MARGIN, AUTO_CONFIDENCE, BackendScore, Detection, DetectionOutcome, DetectionSource,
    MAX_SEARCH_DEPTH, MIN_CONFIDENCE, Signal, classify, detect_from, detect_from_known,
    score_directory,
};
pub use scaffold::{Scaffold, ScaffoldFile};
pub use scan::{DirScan, TEXT_FILES};

/// Owns the current project and the backends it could belong to.
///
/// Detection is explicit: nothing re-detects behind the user's back, so the
/// displayed evidence always matches the displayed conclusion.
pub struct ProjectManager {
    registry: BackendRegistry,
    /// Projects the user config already names, consulted by detection the
    /// same way a project's own `chiptui.toml` is (`SPEC.md` §7).
    known: crate::settings::ProjectRegistry,
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
            known: crate::settings::ProjectRegistry::default(),
            start_dir: start_dir.into(),
            override_kind: None,
            detection: None,
        }
    }

    pub fn registry(&self) -> &BackendRegistry {
        &self.registry
    }

    /// Replaces the recorded projects detection consults --- reloaded
    /// whenever the config the app reads changes ([`crate::App::set_home_dir`]).
    pub fn set_known_projects(&mut self, known: crate::settings::ProjectRegistry) {
        self.known = known;
    }

    pub fn known_projects(&self) -> &crate::settings::ProjectRegistry {
        &self.known
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
        let mut detection = detect_from_known(&self.registry, &self.start_dir, &self.known)?;
        if let Some(kind) = self.override_kind {
            detection = detection.overridden_with(kind);
        }
        Ok(self.detection.insert(detection))
    }

    /// Sets or clears the manual backend override (`AGENTS.md` §4).
    ///
    /// Clearing restores the automatic conclusion from the evidence already
    /// gathered --- no filesystem access needed. When there was no manual
    /// override to clear (the conclusion already came from scoring or from
    /// the persisted `chiptui.toml`, `SPEC.md` §7), this is a no-op: without
    /// the guard, re-deriving the outcome via `classify` would discard a
    /// scaffold-file conclusion that was never based on the raw scores in
    /// the first place (`detect_from` lets the scaffold win outright, ahead
    /// of the confidence check), turning a correctly detected backend into
    /// "unknown" just for picking "Automatic" in the picker.
    pub fn set_override(&mut self, kind: Option<BackendKind>) {
        let had_override = self.override_kind.is_some();
        self.override_kind = kind;
        let Some(detection) = self.detection.take() else {
            return;
        };
        self.detection = Some(match kind {
            Some(kind) => detection.overridden_with(kind),
            None if had_override => Detection {
                outcome: classify(&detection.scores),
                source: DetectionSource::Automatic,
                ..detection
            },
            None => detection,
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

    /// Directory the empty-project scaffold operates on: the detected root,
    /// or [`ProjectManager::start_dir`] before detection has produced one
    /// --- the empty-project prompt this backs can fire before a root is
    /// known.
    pub fn scaffold_dir(&self) -> &Path {
        self.root().unwrap_or(self.start_dir.as_path())
    }

    /// Lays down `kind`'s starting layout in [`Self::scaffold_dir`]
    /// (`SPEC.md` §7): the backend declares it
    /// ([`crate::backend::Backend::scaffold`]), this writes it, and nothing
    /// already in the directory is overwritten.
    ///
    /// Which backend the directory *is* is recorded in the user config by
    /// the caller ([`crate::App::record_open_project`]), not in a file
    /// inside the project --- ChipTUI reads a project's `chiptui.toml` when
    /// one exists, but no longer creates one.
    pub fn create_scaffold(&self, kind: BackendKind) -> io::Result<scaffold::Created> {
        let dir = self.scaffold_dir();
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string());
        let Some(backend) = self.registry.get(kind) else {
            return Ok(scaffold::Created::default());
        };
        scaffold::create(dir, &backend.scaffold(&name))
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
    fn selecting_automatic_with_no_active_override_is_a_no_op() {
        // Regression: pressing 'o' and choosing "Automatic" always called
        // `set_override(None)`, even when nothing was overridden. That used
        // to re-derive the outcome from raw scores unconditionally, which
        // silently downgraded a `chiptui.toml`-backed conclusion (weak or
        // empty scores, since the scaffold file wins outright ahead of the
        // confidence check) to Unknown.
        let mut manager = manager_with_detection();
        manager.detection.as_mut().unwrap().scores.clear();
        manager.detection.as_mut().unwrap().source = DetectionSource::Config;

        assert_eq!(manager.override_kind(), None, "nothing was ever overridden");
        manager.set_override(None);

        assert_eq!(
            manager.selected_kind(),
            Some(BackendKind::MicroPython),
            "a config-sourced conclusion must survive picking 'Automatic'"
        );
        assert_eq!(manager.detection().unwrap().source, DetectionSource::Config);
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

    #[test]
    fn create_scaffold_falls_back_to_start_dir_before_detection_ran() {
        let dir =
            std::env::temp_dir().join(format!("chiptui-manager-scaffold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let manager = ProjectManager::new(&dir);
        assert!(manager.root().is_none(), "nothing detected yet");
        manager.create_scaffold(BackendKind::MicroPython).unwrap();

        let src = dir.join("src/main.py").is_file();
        let firmware = dir.join("firmware").is_dir();
        let marker = dir.join(config::FILE_NAME).exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(src, "the entry point was not written");
        assert!(firmware, "firmware/ was not created");
        assert!(!marker, "the project directory gets no config file of ours");
    }

    #[test]
    fn create_scaffold_leaves_existing_sources_alone() {
        let dir = std::env::temp_dir().join(format!(
            "chiptui-manager-scaffold-idempotent-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("boot.py"), "mine\n").unwrap();

        let manager = ProjectManager::new(&dir);
        let created = manager.create_scaffold(BackendKind::MicroPython).unwrap();

        let boot = std::fs::read_to_string(dir.join("src/boot.py")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(boot, "mine\n", "an existing file must not be touched");
        assert_eq!(created.skipped, vec![PathBuf::from("src/boot.py")]);
    }
}
