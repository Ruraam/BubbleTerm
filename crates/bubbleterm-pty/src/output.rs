//! Bounded, background PTY output reader.
//!
//! Reading a PTY master can block until the child writes or exits. This
//! adapter moves that blocking read off the UI/event-loop thread and applies
//! backpressure with a bounded channel.

use std::{
    io::{self, Read},
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result};

use crate::PtySession;

/// One item produced by the PTY output worker.
#[derive(Debug, Eq, PartialEq)]
pub enum PtyOutputEvent {
    /// Bytes read from the PTY master, in stream order.
    Data(Vec<u8>),
    /// The PTY stream reached its normal end.
    Closed,
    /// A non-terminal I/O failure occurred while reading.
    Error(String),
}

/// Receives PTY output without performing blocking reads on the caller thread.
///
/// The channel is bounded to limit memory use when the UI is temporarily
/// slower than the child process. The worker naturally pauses while the
/// consumer catches up.
pub struct PtyOutputReader {
    receiver: Option<Receiver<PtyOutputEvent>>,
    // Joined during Drop after the owning PtySession has terminated its child.
    worker: Option<JoinHandle<()>>,
}

impl Drop for PtyOutputReader {
    fn drop(&mut self) {
        // Close the receiving side first so a worker blocked on the bounded
        // channel wakes up. WindowState drops its PtySession before this
        // reader, which kills/reaps the child and unblocks a pending PTY read.
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl PtyOutputReader {
    /// Start a dedicated reader thread for an existing PTY session.
    pub fn spawn(session: &PtySession) -> Result<Self> {
        let mut reader = session
            .try_clone_reader()
            .context("could not clone the PTY output reader")?;
        let (sender, receiver) = mpsc::sync_channel(16);
        let worker = thread::Builder::new()
            .name("bubbleterm-pty-output".to_owned())
            .spawn(move || read_loop(&mut reader, sender))
            .context("could not start the PTY output worker")?;

        Ok(Self {
            receiver: Some(receiver),
            worker: Some(worker),
        })
    }

    /// Receive the next event without waiting.
    pub fn try_recv(&self) -> std::result::Result<PtyOutputEvent, TryRecvError> {
        self.receiver
            .as_ref()
            .map_or(Err(TryRecvError::Disconnected), Receiver::try_recv)
    }

    /// Receive the next event, waiting for at most the supplied duration.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<PtyOutputEvent, RecvTimeoutError> {
        self.receiver
            .as_ref()
            .map_or(Err(RecvTimeoutError::Disconnected), |receiver| {
                receiver.recv_timeout(timeout)
            })
    }
}

fn read_loop(reader: &mut dyn Read, sender: SyncSender<PtyOutputEvent>) {
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(PtyOutputEvent::Closed);
                break;
            }
            Ok(count) => {
                if sender
                    .send(PtyOutputEvent::Data(buffer[..count].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) if is_normal_pty_close(&error) => {
                let _ = sender.send(PtyOutputEvent::Closed);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.send(PtyOutputEvent::Error(error.to_string()));
                break;
            }
        }
    }
}

#[cfg(unix)]
fn is_normal_pty_close(error: &io::Error) -> bool {
    // Unix PTY masters commonly report EIO after the slave side has closed.
    error.raw_os_error() == Some(5)
}

#[cfg(not(unix))]
fn is_normal_pty_close(_: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{PtyOutputEvent, PtyOutputReader};
    use crate::{LaunchSpec, PtySession, ShellKind};
    use portable_pty::PtySize;
    use std::time::Duration;

    #[test]
    fn dropping_session_unblocks_and_joins_the_output_worker() {
        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "exec sleep 30".into()],
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
        let output = PtyOutputReader::spawn(&session).expect("reader should start");

        // Terminating the PTY child closes the slave side, waking the
        // blocking reader. Dropping the reader then joins its worker.
        drop(session);
        drop(output);
    }

    #[test]
    fn receives_child_output_off_the_caller_thread() {
        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "printf 'bubbleterm-output-ok\\n'".into()],
            working_directory: None,
            environment: Default::default(),
            kind: ShellKind::Host,
        };
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut session = PtySession::spawn(&spec, size).expect("PTY should spawn");
        let output = PtyOutputReader::spawn(&session).expect("output reader should start");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut bytes = Vec::new();
        while !bytes
            .windows(b"bubbleterm-output-ok".len())
            .any(|window| window == b"bubbleterm-output-ok")
        {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "child output timed out");
            match output
                .recv_timeout(remaining)
                .expect("output event timed out")
            {
                PtyOutputEvent::Data(chunk) => bytes.extend_from_slice(&chunk),
                PtyOutputEvent::Closed => panic!("PTY closed before expected output arrived"),
                PtyOutputEvent::Error(error) => panic!("PTY reader failed: {error}"),
            }
        }

        let _ = session.wait();
        drop(output);
    }
}
