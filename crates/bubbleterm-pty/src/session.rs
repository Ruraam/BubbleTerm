//! Native PTY allocation, process spawning, and resize propagation.
//!
//! The Wayland configure handler should call PtySession::resize with the
//! newly computed cell and pixel dimensions. The kernel updates the PTY
//! winsize and delivers SIGWINCH to the foreground process group when the
//! slave is its controlling terminal; BubbleTerm must not synthesize SIGWINCH.

use std::io::{Read, Write};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use crate::{LaunchSpec, ShellKind};

/// A running child attached to a PTY master.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // portable-pty starts the command as a session and process-group
        // leader. Signal that whole group so ordinary background children
        // (which inherit the shell's process group) do not outlive the UI.
        // Keep the leader unreaped until group signaling is complete: its PID
        // cannot then be recycled for an unrelated process group.
        let child_running = !matches!(self.child.try_wait(), Ok(Some(_)));

        #[cfg(unix)]
        if child_running {
            if let Some(pid) = self
                .child
                .process_id()
                .and_then(|pid| i32::try_from(pid).ok())
            {
                let process_group = nix::unistd::Pid::from_raw(-pid);
                let _ = nix::sys::signal::kill(process_group, nix::sys::signal::Signal::SIGHUP);
                std::thread::sleep(std::time::Duration::from_millis(50));
                let _ = nix::sys::signal::kill(process_group, nix::sys::signal::Signal::SIGKILL);
            }
        }

        // portable-pty remains the authority for terminating and reaping the
        // direct child. Do not call kill() after try_wait reported it exited:
        // that could signal an unrelated process if the PID were reused.
        if child_running {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
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
        self.master.try_clone_reader()
    }

    /// Clone a PTY reader and an independently owned descriptor for readiness polling.
    ///
    /// The duplicate descriptor remains valid if the session is dropped before
    /// the reader. It lets the output worker use a bounded poll interval so
    /// cancellation never depends on the child producing output.
    #[cfg(unix)]
    pub(crate) fn try_clone_reader_with_poll_fd(
        &self,
    ) -> Result<(Box<dyn Read + Send>, std::os::fd::OwnedFd)> {
        use std::os::fd::{FromRawFd, OwnedFd};

        let reader = self.master.try_clone_reader()?;
        let master_fd = self
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow::anyhow!("PTY master does not expose a file descriptor"))?;
        // SAFETY: dup returns a new owned descriptor on success.
        let poll_fd = unsafe { libc::dup(master_fd) };
        anyhow::ensure!(
            poll_fd >= 0,
            "could not duplicate PTY descriptor: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: ownership of the newly duplicated descriptor is transferred here.
        let poll_fd = unsafe { OwnedFd::from_raw_fd(poll_fd) };
        Ok((reader, poll_fd))
    }

    /// Take the single writer for sending keyboard input to the PTY.
    pub fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
        self.master.take_writer()
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
        self.master.get_size()
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
            assert!(
                !remaining.is_zero(),
                "child did not confirm its controlling TTY"
            );
            let chunk = rx.recv_timeout(remaining).expect("PTY output timed out");
            output.extend_from_slice(&chunk);
        }

        session
            .resize(resized)
            .expect("kernel PTY resize should succeed");

        let winch_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !output.windows(5).any(|window| window == b"WINCH") {
            let remaining = winch_deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "foreground process group did not receive SIGWINCH"
            );
            let chunk = rx
                .recv_timeout(remaining)
                .expect("SIGWINCH response timed out");
            output.extend_from_slice(&chunk);
        }

        session.kill().expect("test child should be terminated");
        let _ = session.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropping_session_kills_background_processes_that_ignore_sighup() {
        use std::{
            fs, thread,
            time::{Duration, Instant, SystemTime, UNIX_EPOCH},
        };

        let pid_file = std::env::temp_dir().join(format!(
            "bubbleterm-descendant-{}-{}.pid",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "trap '' HUP; sleep 30 & child=$!; printf '%s\\n' \"$child\" > \"$BUBBLETERM_TEST_CHILD_PID_FILE\"; wait".into(),
            ],
            working_directory: None,
            environment: [(
                std::ffi::OsString::from("BUBBLETERM_TEST_CHILD_PID_FILE"),
                pid_file.as_os_str().to_owned(),
            )]
            .into_iter()
            .collect(),
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
        .expect("PTY shell should spawn");

        let deadline = Instant::now() + Duration::from_secs(3);
        let child_pid = loop {
            if let Ok(contents) = fs::read_to_string(&pid_file) {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "background child did not publish its PID"
            );
            thread::sleep(Duration::from_millis(10));
        };

        drop(session);

        // A killed process may briefly remain as a zombie until its new
        // parent reaps it. Both absence and zombie state mean it is no longer
        // executing; a live state would indicate a leaked descendant.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let stat_path = format!("/proc/{child_pid}/stat");
            match fs::read_to_string(&stat_path) {
                Ok(stat) => {
                    let state = stat
                        .rsplit_once(')')
                        .and_then(|(_, fields)| fields.split_whitespace().next())
                        .and_then(|field| field.chars().next());
                    if state == Some('Z') {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "background process {child_pid} survived PtySession drop (state: {state:?})"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => panic!("could not inspect background process: {error}"),
            }
            thread::sleep(Duration::from_millis(10));
        }

        let _ = fs::remove_file(pid_file);
    }

    #[test]
    fn try_wait_reports_a_naturally_exited_child() {
        use std::time::{Duration, Instant};

        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 23".into()],
            working_directory: None,
            environment: Default::default(),
            kind: ShellKind::Host,
        };
        let mut session = PtySession::spawn(
            &spec,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("PTY child should spawn");

        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = session.try_wait().expect("try_wait should succeed") {
                break status;
            }
            assert!(Instant::now() < deadline, "child did not exit naturally");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.exit_code(), 23);
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

    #[cfg(unix)]
    #[test]
    fn bubblewrap_session_keeps_controlling_tty_and_delivers_sigwinch() {
        use std::sync::mpsc;
        use std::time::Duration;

        let Ok(rootfs) = std::env::var("BUBBLETERM_TEST_BWRAP_ROOTFS") else {
            eprintln!("skipping Bubblewrap runtime test: test rootfs is not configured");
            return;
        };
        let rootfs = std::path::PathBuf::from(rootfs);
        assert!(
            rootfs.join("bin/sh").exists(),
            "test rootfs must contain /bin/sh"
        );

        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "test -t 0 || exit 31; trap 'echo BUBBLE_WINCH' WINCH; echo BUBBLE_READY; while :; do sleep 0.05; done".into(),
            ],
            working_directory: Some("/".into()),
            environment: Default::default(),
            kind: ShellKind::Bubble {
                rootfs,
                read_only: true,
            },
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
            .expect("Bubblewrap shell should start with a controlling TTY");
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
        let ready_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !output
            .windows(b"BUBBLE_READY".len())
            .any(|window| window == b"BUBBLE_READY")
        {
            let remaining = ready_deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "Bubblewrap child did not confirm its controlling TTY"
            );
            let chunk = rx.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!(
                    "Bubblewrap PTY output timed out ({error}); captured output: {}",
                    String::from_utf8_lossy(&output)
                )
            });
            output.extend_from_slice(&chunk);
        }

        session
            .resize(resized)
            .expect("Bubblewrap PTY resize should succeed");

        let winch_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !output
            .windows(b"BUBBLE_WINCH".len())
            .any(|window| window == b"BUBBLE_WINCH")
        {
            let remaining = winch_deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "Bubblewrap foreground process did not receive SIGWINCH"
            );
            let chunk = rx
                .recv_timeout(remaining)
                .expect("Bubblewrap SIGWINCH response timed out");
            output.extend_from_slice(&chunk);
        }

        session
            .kill()
            .expect("Bubblewrap test child should be terminated");
        let _ = session.wait();
    }
}
