//! Native PTY allocation, process spawning, and resize propagation.
//!
//! The Wayland configure handler should call PtySession::resize with the
//! newly computed cell and pixel dimensions. The kernel updates the PTY
//! winsize and delivers SIGWINCH to the foreground process group when the
//! slave is its controlling terminal; BubbleTerm must not synthesize SIGWINCH.

use std::io::{Read, Write};

use anyhow::Result;
use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, PtySize, PtySystem, native_pty_system,
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
        Ok(self.master.try_clone_reader()?)
    }

    /// Take the single writer for sending keyboard input to the PTY.
    pub fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
        Ok(self.master.take_writer()?)
    }

    /// Update the kernel PTY winsize. The kernel sends SIGWINCH to the
    /// foreground process group attached to the controlling terminal.
    pub fn resize(&self, size: PtySize) -> Result<()> {
        anyhow::ensure!(size.rows > 0 && size.cols > 0, "PTY rows and columns must be non-zero");
        self.master.resize(size)?;
        Ok(())
    }

    /// Return the winsize currently stored by the kernel.
    pub fn size(&self) -> Result<PtySize> {
        Ok(self.master.get_size()?)
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
        assert_eq!(session.size().expect("PTY size should be readable"), initial);

        session.resize(resized).expect("PTY resize should succeed");
        assert_eq!(session.size().expect("PTY size should be readable"), resized);
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
