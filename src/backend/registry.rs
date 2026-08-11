//! Backend registry.
//!
//! The single place that knows which backends exist. Everything else --- the
//! detector, the project manager, the UI --- goes through it, so adding a
//! backend does not mean touching the UI.

use crate::backend::micropython::MicroPythonBackend;
use crate::backend::zephyr::ZephyrBackend;
use crate::backend::{Backend, BackendKind, Capabilities, tool_available};

pub struct BackendRegistry {
    backends: Vec<Box<dyn Backend>>,
}

impl BackendRegistry {
    pub fn with_builtin_backends() -> Self {
        Self {
            backends: vec![Box::new(MicroPythonBackend), Box::new(ZephyrBackend)],
        }
    }

    pub fn backends(&self) -> impl Iterator<Item = &dyn Backend> {
        self.backends.iter().map(AsRef::as_ref)
    }

    pub fn get(&self, kind: BackendKind) -> Option<&dyn Backend> {
        self.backends().find(|backend| backend.kind() == kind)
    }

    /// Capabilities of `kind`, or an empty set if no backend is selected.
    pub fn capabilities(&self, kind: Option<BackendKind>) -> Capabilities {
        kind.and_then(|kind| self.get(kind))
            .map_or(Capabilities::empty(), |backend| backend.capabilities())
    }

    /// Required tools of `kind` paired with whether they are on `PATH`.
    ///
    /// A capability whose tool is missing cannot actually be used, so the UI
    /// reports this next to the capability list (`SPEC.md` §13).
    pub fn tool_status(&self, kind: BackendKind) -> Vec<(&'static str, bool)> {
        self.get(kind).map_or_else(Vec::new, |backend| {
            backend
                .required_tools()
                .iter()
                .map(|tool| (*tool, tool_available(tool)))
                .collect()
        })
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::with_builtin_backends()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_backend_kind_is_registered_exactly_once() {
        let registry = BackendRegistry::with_builtin_backends();
        for kind in BackendKind::ALL {
            assert_eq!(
                registry.backends().filter(|b| b.kind() == *kind).count(),
                1,
                "{kind} must be registered exactly once"
            );
        }
        assert_eq!(registry.backends().count(), BackendKind::ALL.len());
    }

    #[test]
    fn every_backend_declares_capabilities_and_tools() {
        let registry = BackendRegistry::with_builtin_backends();
        for backend in registry.backends() {
            assert!(
                !backend.capabilities().is_empty(),
                "{} declares no capabilities",
                backend.kind()
            );
            assert!(
                !backend.required_tools().is_empty(),
                "{} declares no tools",
                backend.kind()
            );
            assert!(
                backend.saturation() > 0.0,
                "{} has no saturation",
                backend.kind()
            );
        }
    }

    #[test]
    fn capabilities_of_no_selection_is_empty() {
        let registry = BackendRegistry::with_builtin_backends();
        assert!(registry.capabilities(None).is_empty());
        assert!(!registry.capabilities(Some(BackendKind::Zephyr)).is_empty());
    }

    #[test]
    fn tool_status_lists_every_required_tool() {
        let registry = BackendRegistry::with_builtin_backends();
        let status = registry.tool_status(BackendKind::MicroPython);
        assert_eq!(
            status.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            vec!["mpremote", "esptool"]
        );
    }
}
