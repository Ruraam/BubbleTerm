//! Declarative process launch policy.
//!
//! This module deliberately describes intent; it does not construct a shell
//! command line. The backend must pass every argument as an OS argument (never
//! through `sh -c`) and must fail closed when sandboxing is requested but
//! Bubblewrap or a required namespace is unavailable.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

/// Execution environment requested for a terminal session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Run the selected shell as the current user on the host.
    Host,
    /// Run the selected shell inside a Bubblewrap-created namespace.
    Sandbox(SandboxConfig),
}

/// Network access available inside a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// Do not create a network namespace; networking remains available.
    Host,
    /// Create a private network namespace with no host network access.
    Isolated,
}

/// How a host path is exposed inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMountKind {
    ReadOnly,
    ReadWrite,
}

/// An explicit host-to-sandbox path mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxMount {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub kind: SandboxMountKind,
}

/// Bubblewrap policy. No host filesystem paths are exposed implicitly by this
/// structure; the backend supplies only the minimal runtime mounts it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    /// Executable name or absolute path for Bubblewrap.
    pub bwrap_program: PathBuf,
    /// Whether the sandbox receives host networking.
    pub network: NetworkPolicy,
    /// Additional explicit filesystem mappings.
    pub mounts: Vec<SandboxMount>,
    /// Create a private PID namespace.
    pub private_pid: bool,
    /// Create a private IPC namespace.
    pub private_ipc: bool,
    /// Create a private UTS namespace.
    pub private_uts: bool,
    /// Mount a fresh temporary directory at /tmp.
    pub private_tmp: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            bwrap_program: PathBuf::from("bwrap"),
            network: NetworkPolicy::Isolated,
            mounts: Vec::new(),
            private_pid: true,
            private_ipc: true,
            private_uts: true,
            private_tmp: true,
        }
    }
}

/// Complete, backend-independent description of a shell session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub shell: PathBuf,
    pub args: Vec<OsString>,
    pub working_directory: Option<PathBuf>,
    pub environment: BTreeMap<OsString, OsString>,
    pub mode: ExecutionMode,
}

impl LaunchSpec {
    /// Reject malformed launch policies before allocating a PTY or spawning.
    pub fn validate(&self) -> Result<(), LaunchSpecError> {
        if self.shell.as_os_str().is_empty() {
            return Err(LaunchSpecError::EmptyShell);
        }

        if let ExecutionMode::Sandbox(config) = &self.mode {
            if config.bwrap_program.as_os_str().is_empty() {
                return Err(LaunchSpecError::EmptyBubblewrapProgram);
            }

            for mount in &config.mounts {
                if !mount.source.is_absolute() {
                    return Err(LaunchSpecError::MountSourceNotAbsolute(
                        mount.source.clone(),
                    ));
                }
                if !mount.destination.is_absolute() {
                    return Err(LaunchSpecError::MountDestinationNotAbsolute(
                        mount.destination.clone(),
                    ));
                }
            }
        }

        Ok(())
    }
}

/// Invalid launch configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSpecError {
    EmptyShell,
    EmptyBubblewrapProgram,
    MountSourceNotAbsolute(PathBuf),
    MountDestinationNotAbsolute(PathBuf),
}

impl std::fmt::Display for LaunchSpecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyShell => formatter.write_str("shell executable cannot be empty"),
            Self::EmptyBubblewrapProgram => {
                formatter.write_str("Bubblewrap executable cannot be empty")
            }
            Self::MountSourceNotAbsolute(path) => {
                write!(formatter, "sandbox mount source must be absolute: {}", path.display())
            }
            Self::MountDestinationNotAbsolute(path) => write!(
                formatter,
                "sandbox mount destination must be absolute: {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LaunchSpecError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_spec() -> LaunchSpec {
        LaunchSpec {
            shell: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-l")],
            working_directory: None,
            environment: BTreeMap::new(),
            mode: ExecutionMode::Host,
        }
    }

    #[test]
    fn accepts_host_shell() {
        assert!(host_spec().validate().is_ok());
    }

    #[test]
    fn rejects_empty_shell() {
        let mut spec = host_spec();
        spec.shell = PathBuf::new();
        assert_eq!(spec.validate(), Err(LaunchSpecError::EmptyShell));
    }

    #[test]
    fn rejects_relative_sandbox_mounts() {
        let mut spec = host_spec();
        spec.mode = ExecutionMode::Sandbox(SandboxConfig {
            mounts: vec![SandboxMount {
                source: PathBuf::from("relative"),
                destination: PathBuf::from("/workspace"),
                kind: SandboxMountKind::ReadOnly,
            }],
            ..SandboxConfig::default()
        });
        assert!(matches!(
            spec.validate(),
            Err(LaunchSpecError::MountSourceNotAbsolute(_))
        ));
    }
}
