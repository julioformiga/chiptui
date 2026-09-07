//! Where a sequence of steps stands, and where the sequence as a whole does.
//!
//! Two enums, extracted from [`crate::install`] when a second multi-step
//! flow appeared. They carry nothing installer-specific --- no command, no
//! path, no step identity --- which is exactly why they generalize: a step
//! is `Pending`, `Running`, `Done`, `Failed` or `Skipped` whether it is
//! `west update` or a Kconfig block being written, and a panel running them
//! is `Idle`, `Running`, `Finished` or `Stopped`.
//!
//! They live here rather than in one flow and get borrowed by the other so
//! that neither reads as depending on the other: preparing a project for OTA
//! does not depend on the Zephyr installer, it merely has the same shape.
//! [`crate::install`] re-exports both, so every existing path still
//! resolves.
//!
//! The renderer's mapping from these to a checklist mark
//! (`ui::workspace::RowMark`) is what makes the two flows *look* the same as
//! well, which is the point: a user who has run the installer already knows
//! how to read the OTA panel.

/// Where a step stands. `Skipped` is an answer, not a failure --- it is
/// drawn differently and never blocks what follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Failed(String),
    Skipped,
}

/// What a stepped panel as a whole is doing, for the state line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Probing requirements, or waiting for the user to start.
    Idle,
    Running,
    /// Every step reached `Done` or `Skipped`.
    Finished,
    /// A step failed; the sequence stopped there.
    Stopped(String),
}
