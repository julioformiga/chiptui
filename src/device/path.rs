//! Paths on the device.
//!
//! Deliberately *not* [`std::path::PathBuf`]: device paths are always absolute
//! and `/`-separated regardless of the host. On Windows a `PathBuf` would join
//! with `\` and produce an argument MicroPython cannot resolve.

use std::fmt;

/// An absolute path on the device's filesystem.
///
/// Stored without a trailing slash (the root is `/`), so joining is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DevicePath(String);

impl DevicePath {
    pub fn root() -> Self {
        Self("/".to_string())
    }

    /// Builds a path from an absolute, `/`-separated string.
    ///
    /// Leading/duplicate/trailing separators are normalised; `.` and `..`
    /// components are resolved so a listing can never escape the root.
    pub fn new(path: &str) -> Self {
        let mut components: Vec<&str> = Vec::new();
        for component in path.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    components.pop();
                }
                name => components.push(name),
            }
        }
        if components.is_empty() {
            return Self::root();
        }
        Self(format!("/{}", components.join("/")))
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// The path with `name` appended.
    pub fn join(&self, name: &str) -> Self {
        if self.is_root() {
            Self::new(&format!("/{name}"))
        } else {
            Self::new(&format!("{}/{name}", self.0))
        }
    }

    /// The containing directory, or `None` at the root.
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let cut = self.0.rfind('/').unwrap_or(0);
        Some(if cut == 0 {
            Self::root()
        } else {
            Self(self.0[..cut].to_string())
        })
    }

    /// The last component, or `/` at the root.
    pub fn name(&self) -> &str {
        if self.is_root() {
            return "/";
        }
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path as an `mpremote` argument: a `:` prefix marks it as remote.
    pub fn as_arg(&self) -> String {
        format!(":{}", self.0)
    }
}

impl fmt::Display for DevicePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_a_single_slash() {
        let root = DevicePath::root();
        assert!(root.is_root());
        assert_eq!(root.as_str(), "/");
        assert_eq!(root.as_arg(), ":/");
        assert_eq!(root.name(), "/");
        assert_eq!(root.parent(), None);
    }

    #[test]
    fn joining_builds_slash_separated_paths() {
        let lib = DevicePath::root().join("lib");
        assert_eq!(lib.as_str(), "/lib");
        assert_eq!(lib.as_arg(), ":/lib");

        let nested = lib.join("umqtt");
        assert_eq!(nested.as_str(), "/lib/umqtt");
        // The separator is never the host's, even on Windows.
        assert!(!nested.as_str().contains('\\'));
    }

    #[test]
    fn parent_walks_back_to_the_root() {
        let deep = DevicePath::new("/lib/umqtt/simple.py");
        let up = deep.parent().unwrap();
        assert_eq!(up.as_str(), "/lib/umqtt");
        assert_eq!(up.parent().unwrap().as_str(), "/lib");
        assert_eq!(up.parent().unwrap().parent().unwrap(), DevicePath::root());
    }

    #[test]
    fn normalisation_collapses_separators_and_dots() {
        assert_eq!(DevicePath::new("//lib//umqtt/").as_str(), "/lib/umqtt");
        assert_eq!(DevicePath::new("lib").as_str(), "/lib");
        assert_eq!(DevicePath::new("/lib/./umqtt").as_str(), "/lib/umqtt");
        assert_eq!(DevicePath::new("").as_str(), "/");
    }

    #[test]
    fn parent_traversal_cannot_escape_the_root() {
        assert_eq!(DevicePath::new("/../../etc").as_str(), "/etc");
        assert_eq!(DevicePath::new("/lib/../..").as_str(), "/");
        assert_eq!(DevicePath::root().join("..").as_str(), "/");
    }

    #[test]
    fn name_returns_the_last_component() {
        assert_eq!(DevicePath::new("/lib/simple.py").name(), "simple.py");
        assert_eq!(DevicePath::new("/boot.py").name(), "boot.py");
    }
}
