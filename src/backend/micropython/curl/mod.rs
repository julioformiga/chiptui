//! `curl` command construction and output parsing.
//!
//! `SPEC.md` §9/§22: fetching the micropython.org download pages and the
//! chosen firmware file are both delegated to `curl` through the existing
//! [`crate::process::ProcessManager`], the same way `esptool`/`mpremote`
//! invocations are --- no bundled HTTP client, no parallel async-ish
//! subsystem. Mirrors the `esptool` submodule split: [`commands`] builds
//! invocations, [`parse`] reads their output.

pub mod commands;
pub mod parse;
