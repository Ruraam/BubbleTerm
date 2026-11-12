//! PTY shell selection and process lifecycle boundary.
//!
//! Host and BubbleOS Bubblewrap sessions expose the same PTY byte-stream
//! contract to the VT engine and UI.

#![forbid(unsafe_code)]

pub mod launch;

pub use launch::{CommandSpec, LaunchSpec, LaunchSpecError, ShellKind};
