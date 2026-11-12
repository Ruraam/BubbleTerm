//! Native PTY allocation, process spawning, and resize propagation.
//!
//! The Wayland configure handler should call PtySession::resize with the
//! newly computed cell and pixel dimensions. The kernel updates the PTY
//! winsize and delivers SIGWINCH to the foreground process group when the
//! slave is its controlling terminal; BubbleTerm must not synthesize SIGWINCH.

use std::io::{Read, Write};

use anyhow::Result;
use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system,
};

use crate::{LaunchSpec, ShellKind};

/// A running child attached to a PTY master.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtySession {
    /// Allocate a PTY and spawn the requested Host or Bubble command.
    ///
    /// portable-pty creates a new session and assigns the slave to the child
    /// as its controlling terminal (TIOCSCTTY on Unix). Bubble mode therefore
    /// deliberately avoids bwrap's --new-session, which would detach it.
    pub fn spawn(spec: &LaunchSpec, size: PtySize) -> Result<Self> {
        spec.validate()?;
        let command_spec = spec.command_spec()?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size)?;

        let mut command = CommandBuilder::new(command_spec.program.as_os_str());
        command.args(command_spec.args.iter());
        command.set_controlling_tty(true);

        if matches!(&spec.kind, ShellKind::Host) {
            if let Some(directory) = &spec.working_directory {
                command.cwd(directory.as_os_str());
            }
            for (key, value) in &spec.environment {
                command.env(key, value);
            }
        }

        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);

        Ok(Self {
            master: pair.master,
            child,
        })
    }

    /// Clone a reader for the PTY output stream.
    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader().map_err(Into::into)
    }

    /// Take the single writer for sending keyboard input to the PTY.
    pub fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
        self.master.take_writer().map_err(Into::into)
    }

    /// Update the kernel PTY winsize. The kernel sends SIGWINCH to the
    /// foreground process group attached to the controlling terminal.
    pub fn resize(&self, size: PtySize) -> Result<()> {
        anyhow::ensure!(
            size.rows > 0 && size.cols > 0,
            "PTY rows and columns must be non-zero"
        );
        self.master.resize(size)?;
        Ok(())
    }

    /// Return the winsize currently stored by the kernel.
    pub fn size(&self) -> Result<PtySize> {
        self.master.get_size().map_err(Into::into)
    }

    /// Poll the child without blocking.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Wait for the child process to exit.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Request child termination through portable-pty's child-killer API.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_updates_the_kernel_pty_winsize() {
        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            working_directory: None,
            environment: Default::default(),
            kind: ShellKind::Host,
        };
        let initial = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
        };
        let resized = PtySize {
            rows: 42,
            cols: 132,
            pixel_width: 1320,
            pixel_height: 840,
        };

        let session = PtySession::spawn(&spec, initial).expect("PTY should spawn");
        assert_eq!(
            session.size().expect("PTY size should be readable"),
            initial
        );

        session.resize(resized).expect("PTY resize should succeed");
        assert_eq!(
            session.size().expect("PTY size should be readable"),
            resized
        );
    }

    #[cfg(unix)]
    #[test]
    fn controlling_tty_receives_sigwinch_after_resize() {
        use std::sync::mpsc;
        use std::time::Duration;

        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "test -t 0 || exit 31; trap 'echo WINCH' WINCH; echo READY; while :; do sleep 0.05; done".into(),
            ],
            working_directory: None,
            environment: Default::default(),
            kind: ShellKind::Host,
        };
        let initial = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
        };
        let resized = PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
        };

        let mut session = PtySession::spawn(&spec, initial)
            .expect("interactive child should receive a controlling TTY");
        let mut reader = session
            .try_clone_reader()
            .expect("PTY reader should be cloneable");
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let mut buffer = [0_u8; 256];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if tx.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut output = Vec::new();
        let ready_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !output.windows(5).any(|window| window == b"READY") {
            let remaining = ready_deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "child did not confirm its controlling TTY");
            let chunk = rx.recv_timeout(remaining).expect("PTY output timed out");
            output.extend_from_slice(&chunk);
        }

        session
            .resize(resized)
            .expect("kernel PTY resize should succeed");

        let winch_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !output.windows(5).any(|window| window == b"WINCH") {
            let remaining = winch_deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "foreground process group did not receive SIGWINCH");
            let chunk = rx.recv_timeout(remaining).expect("SIGWINCH response timed out");
            output.extend_from_slice(&chunk);
        }

        session.kill().expect("test child should be terminated");
        let _ = session.wait();
    }

    #[test]
    fn resize_rejects_zero_cell_dimensions() {
        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            working_directory: None,
            environment: Default::default(),
            kind: ShellKind::Host,
        };
        let session = PtySession::spawn(
            &spec,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("PTY should spawn");

        let result = session.resize(PtySize {
            rows: 0,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        assert!(result.is_err());
    }
}
