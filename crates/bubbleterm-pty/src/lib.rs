//! PTY launch policy and process lifecycle boundary.
//!
//! Host and Bubblewrap launches share one PTY contract. The VT engine and UI
//! consume the resulting PTY stream without knowing which execution mode was
//! selected. Actual process spawning belongs behind this crate's backend.

#![forbid(unsafe_code)]

pub mod launch;

pub use launch::{
    ExecutionMode, LaunchSpec, NetworkPolicy, SandboxConfig, SandboxMount,
    SandboxMountKind,
};
