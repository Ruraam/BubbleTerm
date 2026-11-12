//! PTY shell selection, process lifecycle, and terminal-size propagation.
//!
//! Host and BubbleOS Bubblewrap sessions expose the same PTY byte-stream
//! contract to the VT engine and UI.

#![forbid(unsafe_code)]

pub mod launch;
pub mod session;

pub use launch::{CommandSpec, LaunchSpec, LaunchSpecError, ShellKind};
pub use portable_pty::PtySize;
pub use session::PtySession;
