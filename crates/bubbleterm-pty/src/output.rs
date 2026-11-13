//! Bounded, background PTY output reader.
//!
//! Reading a PTY master can block until the child writes or exits. This
//! adapter moves that blocking read off the UI/event-loop thread and applies
//! backpressure with a bounded channel.

use std::{
    io::{self, Read},
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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
    cancellation: Arc<AtomicBool>,
}

impl Drop for PtyOutputReader {
    fn drop(&mut self) {
        // Close the receiving side first so a worker blocked on the bounded
        // channel wakes up. On Unix, nonblocking reads observe cancellation
        // while the child is silent. Other platforms still rely on the owning
        // session being dropped first to interrupt a blocking PTY read.
        self.receiver.take();
        self.cancellation.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl PtyOutputReader {
    /// Start a dedicated reader thread for an existing PTY session.
    pub fn spawn(session: &PtySession) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(16);
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);

        #[cfg(unix)]
        let worker = {
            let mut reader = session
                .try_clone_nonblocking_reader()
                .context("could not clone the PTY output reader")?;
            thread::Builder::new()
                .name("bubbleterm-pty-output".to_owned())
                .spawn(move || read_loop_unix(&mut reader, sender, worker_cancellation))
                .context("could not start the PTY output worker")?
        };

        #[cfg(not(unix))]
        let worker = {
            let mut reader = session
                .try_clone_reader()
                .context("could not clone the PTY output reader")?;
            thread::Builder::new()
                .name("bubbleterm-pty-output".to_owned())
                .spawn(move || read_loop(&mut reader, sender, worker_cancellation))
                .context("could not start the PTY output worker")?
        };

        Ok(Self {
            receiver: Some(receiver),
            worker: Some(worker),
            cancellation,
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

#[cfg(unix)]
fn read_loop_unix(
    reader: &mut dyn Read,
    sender: SyncSender<PtyOutputEvent>,
    cancellation: Arc<AtomicBool>,
) {
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        if cancellation.load(Ordering::Acquire) {
            break;
        }
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = sender.send(PtyOutputEvent::Error(error.to_string()));
                break;
            }
        }
    }
}

#[cfg(not(unix))]
fn read_loop(
    reader: &mut dyn Read,
    sender: SyncSender<PtyOutputEvent>,
    cancellation: Arc<AtomicBool>,
) {
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        if cancellation.load(Ordering::Acquire) {
            break;
        }
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
    fn reports_closed_after_child_exits() {
        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "printf 'before-exit\\n'; exit 0".into()],
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
        let output = PtyOutputReader::spawn(&session).expect("output reader should start");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut bytes = Vec::new();
        let mut closed = false;
        while !closed {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "PTY reader did not report closure");
            match output.recv_timeout(remaining).expect("PTY event timed out") {
                PtyOutputEvent::Data(chunk) => bytes.extend_from_slice(&chunk),
                PtyOutputEvent::Closed => closed = true,
                PtyOutputEvent::Error(error) => panic!("PTY reader failed: {error}"),
            }
        }
        assert!(
            bytes
                .windows(b"before-exit".len())
                .any(|window| window == b"before-exit")
        );
        drop(output);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_reader_unblocks_worker_when_child_is_silent() {
        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "exec sleep 30".into()],
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
        .expect("PTY should spawn");
        let output = PtyOutputReader::spawn(&session).expect("output reader should start");

        let started = std::time::Instant::now();
        drop(output);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "dropping a silent reader should not wait for child output"
        );
        assert!(
            session
                .try_wait()
                .expect("child status should be readable")
                .is_none(),
            "dropping reader must not terminate child"
        );
    }

    #[test]
    fn dropping_reader_unblocks_worker_when_output_channel_is_full() {
        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "exec yes bubbleterm".into()],
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
        .expect("PTY should spawn");
        let output = PtyOutputReader::spawn(&session).expect("output reader should start");
        std::thread::sleep(Duration::from_millis(150));
        let started = std::time::Instant::now();
        drop(output);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "dropping receiver should unblock the output worker"
        );
        assert!(
            session
                .try_wait()
                .expect("child status should be readable")
                .is_none(),
            "dropping reader must not terminate child"
        );
        drop(session);
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

    #[cfg(unix)]
    #[test]
    fn reports_non_terminal_read_errors_once() {
        use std::{
            io,
            sync::{
                Arc,
                atomic::AtomicBool,
                mpsc,
            },
        };

        struct FailingReader;

        impl std::io::Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulated PTY read failure",
                ))
            }
        }

        let (sender, receiver) = mpsc::sync_channel(2);
        super::read_loop_unix(
            &mut FailingReader,
            sender,
            Arc::new(AtomicBool::new(false)),
        );

        assert_eq!(
            receiver.recv().expect("reader should report its I/O failure"),
            PtyOutputEvent::Error("simulated PTY read failure".to_owned())
        );
        assert!(
            receiver.try_recv().is_err(),
            "a terminal read failure must end the worker without duplicate events"
        );
    }
}
