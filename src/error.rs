//! Error types.
//!
//! Categories follow `SPEC.md` §14, but only the ones actually produced today
//! exist. New categories are added when the operation that can fail is added.

use std::fmt;
use std::io;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The process is not attached to a terminal, so the TUI cannot start.
    NotATerminal,
    /// Failed while reading the project tree during detection.
    ProjectScan { path: PathBuf, source: io::Error },
    /// Terminal setup/teardown or event-loop I/O failure.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotATerminal => write!(
                f,
                "standard output is not a terminal; ChipTUI needs an interactive terminal to run"
            ),
            Self::ProjectScan { path, source } => {
                write!(
                    f,
                    "could not read {} during project detection: {source}",
                    path.display()
                )
            }
            Self::Io(source) => write!(f, "terminal I/O failed: {source}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotATerminal => None,
            Self::ProjectScan { source, .. } | Self::Io(source) => Some(source),
        }
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}
