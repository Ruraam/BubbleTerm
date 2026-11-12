//! Declarative shell selection and safe command construction for PTY sessions.
//!
//! Bubble mode invokes Bubblewrap directly with OS arguments. No intermediate
//! shell is used, and a requested sandbox is never downgraded to host mode.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

/// Shell execution target. BubbleOS root filesystems are mounted at /.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellKind {
    /// Execute the shell directly on the host.
    Host,
    /// Execute the shell inside a Bubblewrap root filesystem.
    Bubble { rootfs: PathBuf, read_only: bool },
}

/// Complete description of a shell session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Shell executable. In Bubble mode this is a path inside the rootfs.
    pub shell: PathBuf,
    pub args: Vec<OsString>,
    /// Working directory on the host in Host mode, inside rootfs in Bubble mode.
    pub working_directory: Option<PathBuf>,
    pub environment: BTreeMap<OsString, OsString>,
    pub kind: ShellKind,
}

/// Program and arguments to pass directly to the process-spawn API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl LaunchSpec {
    /// Validate inputs before allocating a PTY or spawning a process.
    pub fn validate(&self) -> Result<(), LaunchSpecError> {
        if self.shell.as_os_str().is_empty() {
            return Err(LaunchSpecError::EmptyShell);
        }

        if let ShellKind::Bubble { rootfs, .. } = &self.kind {
            if !rootfs.is_absolute() {
                return Err(LaunchSpecError::RootfsNotAbsolute(rootfs.clone()));
            }
            if rootfs == &PathBuf::from("/") {
                return Err(LaunchSpecError::RootfsCannotBeHostRoot);
            }
            if !self.shell.is_absolute() {
                return Err(LaunchSpecError::BubbleShellNotAbsolute(self.shell.clone()));
            }
            if let Some(directory) = &self.working_directory {
                if !directory.is_absolute() {
                    return Err(LaunchSpecError::WorkingDirectoryNotAbsolute(
                        directory.clone(),
                    ));
                }
            }
        }

        Ok(())
    }

    /// Build a direct-exec command. This does not spawn it.
    ///
    /// Bubble mode creates private PID/IPC/UTS/network namespaces, mounts the
    /// supplied rootfs at /, and provides fresh /proc, /dev, and /tmp.
    /// The rootfs itself is read-only unless read_only is false.
    pub fn command_spec(&self) -> Result<CommandSpec, LaunchSpecError> {
        self.validate()?;

        match &self.kind {
            ShellKind::Host => Ok(CommandSpec {
                program: self.shell.clone(),
                args: self.args.clone(),
            }),
            ShellKind::Bubble { rootfs, read_only } => {
                let mut args = vec![
                    OsString::from("--die-with-parent"),
                    // portable-pty establishes a new session and assigns the PTY
                    // slave as its controlling terminal before exec. Do not ask
                    // bubblewrap to create another session: setsid() would detach
                    // the controlling TTY and break job control/SIGWINCH delivery.
                    OsString::from("--unshare-pid"),
                    OsString::from("--unshare-ipc"),
                    OsString::from("--unshare-uts"),
                    OsString::from("--unshare-net"),
                    OsString::from(if *read_only { "--ro-bind" } else { "--bind" }),
                    rootfs.as_os_str().to_owned(),
                    OsString::from("/"),
                    OsString::from("--proc"),
                    OsString::from("/proc"),
                    OsString::from("--dev"),
                    OsString::from("/dev"),
                    OsString::from("--tmpfs"),
                    OsString::from("/tmp"),
                ];

                if let Some(directory) = &self.working_directory {
                    args.push(OsString::from("--chdir"));
                    args.push(directory.as_os_str().to_owned());
                }

                for (key, value) in &self.environment {
                    args.push(OsString::from("--setenv"));
                    args.push(key.clone());
                    args.push(value.clone());
                }

                args.push(OsString::from("--"));
                args.push(self.shell.as_os_str().to_owned());
                args.extend(self.args.iter().cloned());

                Ok(CommandSpec {
                    program: PathBuf::from("bwrap"),
                    args,
                })
            }
        }
    }
}

/// Invalid shell launch configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSpecError {
    EmptyShell,
    RootfsNotAbsolute(PathBuf),
    RootfsCannotBeHostRoot,
    BubbleShellNotAbsolute(PathBuf),
    WorkingDirectoryNotAbsolute(PathBuf),
}

impl std::fmt::Display for LaunchSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyShell => f.write_str("shell executable cannot be empty"),
            Self::RootfsNotAbsolute(path) => {
                write!(f, "BubbleOS rootfs must be absolute: {}", path.display())
            }
            Self::RootfsCannotBeHostRoot => {
                f.write_str("BubbleOS rootfs cannot be the host root directory")
            }
            Self::BubbleShellNotAbsolute(path) => {
                write!(f, "sandbox shell path must be absolute: {}", path.display())
            }
            Self::WorkingDirectoryNotAbsolute(path) => {
                write!(f, "working directory must be absolute: {}", path.display())
            }
        }
    }
}

impl std::error::Error for LaunchSpecError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: ShellKind) -> LaunchSpec {
        LaunchSpec {
            shell: PathBuf::from("/bin/bash"),
            args: vec![OsString::from("--login"), OsString::from("-i")],
            working_directory: Some(PathBuf::from("/workspace")),
            environment: BTreeMap::from([(
                OsString::from("TERM"),
                OsString::from("xterm-256color"),
            )]),
            kind,
        }
    }

    #[test]
    fn host_command_is_exact_direct_exec_without_wrapper() {
        let command = spec(ShellKind::Host).command_spec().unwrap();

        assert_eq!(command.program, PathBuf::from("/bin/bash"));
        assert_eq!(
            command.args,
            vec![OsString::from("--login"), OsString::from("-i")]
        );
        assert!(!command.args.iter().any(|arg| arg == "bwrap"));
    }

    #[test]
    fn bubble_read_only_command_has_exact_namespace_mount_and_exec_arguments() {
        let command = spec(ShellKind::Bubble {
            rootfs: PathBuf::from("/var/lib/bubbleos/rootfs"),
            read_only: true,
        })
        .command_spec()
        .unwrap();

        assert_eq!(command.program, PathBuf::from("bwrap"));
        assert_eq!(
            command.args,
            vec![
                "--die-with-parent",
                "--new-session",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
                "--unshare-net",
                "--ro-bind",
                "/var/lib/bubbleos/rootfs",
                "/",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--chdir",
                "/workspace",
                "--setenv",
                "TERM",
                "xterm-256color",
                "--",
                "/bin/bash",
                "--login",
                "-i",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn writable_bubble_command_uses_bind_not_read_only_bind() {
        let command = spec(ShellKind::Bubble {
            rootfs: PathBuf::from("/opt/bubble/rootfs"),
            read_only: false,
        })
        .command_spec()
        .unwrap();

        assert!(command.args.windows(3).any(|args| {
            args == [
                OsString::from("--bind"),
                OsString::from("/opt/bubble/rootfs"),
                OsString::from("/"),
            ]
        }));
        assert!(!command.args.iter().any(|arg| arg == "--ro-bind"));
    }

    #[test]
    fn bubble_command_preserves_shell_arguments_after_separator() {
        let mut launch = spec(ShellKind::Bubble {
            rootfs: PathBuf::from("/opt/bubble/rootfs"),
            read_only: true,
        });
        launch.args = vec![
            OsString::from("-c"),
            OsString::from("printf '%s' 'a b'"),
        ];

        let command = launch.command_spec().unwrap();
        let separator = command
            .args
            .iter()
            .position(|arg| arg == "--")
            .expect("bwrap command must contain an execution separator");

        assert_eq!(
            &command.args[separator + 1..],
            &[
                OsString::from("/bin/bash"),
                OsString::from("-c"),
                OsString::from("printf '%s' 'a b'"),
            ]
        );
    }

    #[test]
    fn rejects_relative_rootfs() {
        assert_eq!(
            spec(ShellKind::Bubble {
                rootfs: PathBuf::from("rootfs"),
                read_only: true
            })
            .validate(),
            Err(LaunchSpecError::RootfsNotAbsolute(PathBuf::from("rootfs")))
        );
    }

    #[test]
    fn rejects_host_root_as_bubble_rootfs() {
        assert_eq!(
            spec(ShellKind::Bubble {
                rootfs: PathBuf::from("/"),
                read_only: true
            })
            .validate(),
            Err(LaunchSpecError::RootfsCannotBeHostRoot)
        );
    }
}
