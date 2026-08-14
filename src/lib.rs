//! ChipTUI --- terminal UI for embedded development workflows.
//!
//! The crate is split so that everything except [`terminal`] and [`ui`] is
//! testable without a terminal: project detection works on directory
//! snapshots ([`project::DirScan`]) and backends expose declarative
//! capabilities instead of framework-specific UI logic.

pub mod app;
pub mod backend;
pub mod browser;
pub mod build;
pub mod console;
pub mod device;
pub mod diff;
pub mod editor;
pub mod error;
pub mod event;
pub mod files;
pub mod flash;
pub mod highlight;
pub mod logs;
pub mod process;
pub mod project;
pub mod terminal;
pub mod ui;

pub use error::{Error, Result};
