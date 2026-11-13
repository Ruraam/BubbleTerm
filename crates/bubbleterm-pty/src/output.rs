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

use crate::{PtySession, session::PtyMasterFd};

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
                .try_clone_reader()
                .context("could not clone the PTY output reader")?;
            let master_fd = session
                .try_clone_raw_fd()
                .context("could not duplicate the PTY master descriptor")?;
            thread::Builder::new()
                .name("bubbleterm-pty-output".to_owned())
                .spawn(move || read_loop_unix(&mut reader, master_fd, sender, worker_cancellation))
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
    master_fd: PtyMasterFd,
    sender: SyncSender<PtyOutputEvent>,
    cancellation: Arc<AtomicBool>,
) {
    use mio::unix::SourceFd;
    use mio::{Events, Interest, Poll, Token};

    let mut poll = match Poll::new() {
        Ok(poll) => poll,
        Err(error) => {
            let _ = sender.send(PtyOutputEvent::Error(error.to_string()));
            return;
        }
    };
    let raw_fd = master_fd.as_raw_fd();
    let mut source = SourceFd(&raw_fd);
    if let Err(error) = poll
        .registry()
        .register(&mut source, Token(0), Interest::READABLE)
    {
        let _ = sender.send(PtyOutputEvent::Error(error.to_string()));
        return;
    }

    let mut events = Events::with_capacity(4);
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        if cancellation.load(Ordering::Acquire) {
            break;
        }

        match poll.poll(&mut events, Some(Duration::from_millis(10))) {
            Ok(()) if events.is_empty() => continue,
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.send(PtyOutputEvent::Error(error.to_string()));
                break;
            }
        }

        if !read_once(reader, &mut buffer, &sender) {
            break;
        }

        // mio uses edge-triggered readiness on supported Unix backends. The
        // bounded channel may block the send above while more PTY bytes arrive.
        // Re-register after each read so data that remained readable during
        // that backpressure is reported again instead of leaving the worker
        // asleep waiting for a new readiness edge.
        if let Err(error) = poll
            .registry()
            .reregister(&mut source, Token(0), Interest::READABLE)
        {
            let _ = sender.send(PtyOutputEvent::Error(error.to_string()));
            break;
        }
    }
}

#[cfg(all(unix, test))]
fn read_loop_unix_unpolled(
    reader: &mut dyn Read,
    sender: SyncSender<PtyOutputEvent>,
    cancellation: Arc<AtomicBool>,
) {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if cancellation.load(Ordering::Acquire) || !read_once(reader, &mut buffer, &sender) {
            break;
        }
    }
}

#[cfg(unix)]
fn read_once(
    reader: &mut dyn Read,
    buffer: &mut [u8],
    sender: &SyncSender<PtyOutputEvent>,
) -> bool {
    match reader.read(buffer) {
        Ok(0) => {
            let _ = sender.send(PtyOutputEvent::Closed);
            false
        }
        Ok(count) => sender
            .send(PtyOutputEvent::Data(buffer[..count].to_vec()))
            .is_ok(),
        Err(error) if is_normal_pty_close(&error) => {
            let _ = sender.send(PtyOutputEvent::Closed);
            false
        }
        Err(error)
            if error.kind() == io::ErrorKind::Interrupted
                || error.kind() == io::ErrorKind::WouldBlock =>
        {
            true
        }
        Err(error) => {
            let _ = sender.send(PtyOutputEvent::Error(error.to_string()));
            false
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
    #[cfg(unix)]
    #[test]
    fn dropping_session_before_reader_unblocks_a_backpressured_worker() {
        const ITERATIONS: usize = 4;

        for iteration in 0..ITERATIONS {
            let spec = LaunchSpec {
                shell: "/bin/sh".into(),
                args: vec!["-c".into(), "exec yes bubbleterm".into()],
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

            // Let the producer fill the bounded channel and block on send.
            std::thread::sleep(Duration::from_millis(100));

            let started = std::time::Instant::now();
            drop(session);
            // Closing the receiver must release a worker that was blocked
            // sending output while the session was being torn down.
            drop(output);

            assert!(
                started.elapsed() < Duration::from_secs(2),
                "session/reader teardown deadlocked on iteration {iteration}"
            );
        }
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
    fn applies_backpressure_without_losing_output_for_slow_consumers() {
        const EXPECTED_BYTES: usize = 1024 * 1024;
        const CONSUMER_DELAY: Duration = Duration::from_millis(2);

        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "dd if=/dev/zero bs=4096 count=256 2>/dev/null | tr '\\000' z".into(),
            ],
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

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut received = Vec::with_capacity(EXPECTED_BYTES);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "slow-consumer PTY read timed out after receiving {} of {EXPECTED_BYTES} bytes",
                received.len()
            );
            match output
                .recv_timeout(remaining)
                .expect("slow-consumer PTY output event timed out")
            {
                PtyOutputEvent::Data(chunk) => {
                    received.extend_from_slice(&chunk);
                    std::thread::sleep(CONSUMER_DELAY);
                }
                PtyOutputEvent::Closed => break,
                PtyOutputEvent::Error(error) => panic!("PTY reader failed: {error}"),
            }
        }

        assert_eq!(
            received.len(),
            EXPECTED_BYTES,
            "bounded-channel backpressure must not discard PTY output"
        );
        assert!(
            received.iter().all(|byte| *byte == b'z'),
            "slow consumption must preserve every byte in the PTY stream"
        );
        drop(output);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_repeated_output_bursts_and_interleaved_markers() {
        const BURST_COUNT: usize = 32;
        const BURST_SIZE: usize = 4096;

        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "i=0; while [ \"$i\" -lt 32 ]; do printf 'BEGIN-%02d\\n' \"$i\"; printf '%4096s' '' | tr ' ' x; printf '\\nEND-%02d\\n' \"$i\"; i=$((i + 1)); sleep 0.01; done".into(),
            ],
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

        let mut expected = Vec::with_capacity(BURST_COUNT * (BURST_SIZE + 24));
        for index in 0..BURST_COUNT {
            expected.extend_from_slice(format!("BEGIN-{index:02}\r\n").as_bytes());
            expected.extend(std::iter::repeat_n(b'x', BURST_SIZE));
            expected.extend_from_slice(format!("\r\nEND-{index:02}\r\n").as_bytes());
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut received = Vec::with_capacity(expected.len());
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "PTY burst output timed out after receiving {} of {} bytes",
                received.len(),
                expected.len()
            );
            match output
                .recv_timeout(remaining)
                .expect("PTY burst output event timed out")
            {
                PtyOutputEvent::Data(chunk) => received.extend_from_slice(&chunk),
                PtyOutputEvent::Closed => break,
                PtyOutputEvent::Error(error) => panic!("PTY reader failed: {error}"),
            }
        }

        assert_eq!(
            received, expected,
            "repeated PTY bursts must preserve every byte, marker, and stream ordering"
        );
        drop(output);
    }

    #[cfg(unix)]
    #[test]
    fn receives_large_contiguous_output_without_losing_bytes() {
        const EXPECTED_BYTES: usize = 1024 * 1024;

        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "dd if=/dev/zero bs=4096 count=256 2>/dev/null | tr '\\000' x".into(),
            ],
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

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut received = Vec::with_capacity(EXPECTED_BYTES);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "large PTY output timed out after receiving {} of {EXPECTED_BYTES} bytes",
                received.len()
            );
            match output
                .recv_timeout(remaining)
                .expect("large PTY output event timed out")
            {
                PtyOutputEvent::Data(chunk) => received.extend_from_slice(&chunk),
                PtyOutputEvent::Closed => break,
                PtyOutputEvent::Error(error) => panic!("PTY reader failed: {error}"),
            }
        }

        assert_eq!(
            received.len(),
            EXPECTED_BYTES,
            "the PTY output stream must deliver every byte before closure"
        );
        assert!(
            received.iter().all(|byte| *byte == b'x'),
            "the PTY output stream must preserve byte contents and order"
        );
        drop(output);
    }

    #[cfg(unix)]
    #[test]
    fn output_reader_preserves_blocking_mode_for_large_pty_writes() {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        use std::io::Write;

        let spec = LaunchSpec {
            shell: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "stty -echo; printf READY; sleep 0.2; cat >/dev/null".into(),
            ],
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

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut ready = false;
        while !ready {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "PTY child did not become ready");
            match output
                .recv_timeout(remaining)
                .expect("PTY output timed out")
            {
                PtyOutputEvent::Data(chunk) => {
                    ready = chunk.windows(5).any(|window| window == b"READY");
                }
                PtyOutputEvent::Closed => panic!("PTY closed before input test"),
                PtyOutputEvent::Error(error) => panic!("PTY reader failed: {error}"),
            }
        }

        let flags = fcntl(
            session.raw_fd().expect("PTY master fd should be available"),
            FcntlArg::F_GETFL,
        )
        .expect("PTY master flags should be readable");
        assert!(
            !OFlag::from_bits_truncate(flags).contains(OFlag::O_NONBLOCK),
            "starting the output reader must not make PTY writes nonblocking"
        );

        let mut payload = Vec::with_capacity(32 * 1024);
        for _ in 0..128 {
            payload.extend(std::iter::repeat_n(b'x', 255));
            payload.push(b'\n');
        }
        let mut writer = session
            .take_writer()
            .expect("PTY writer should be available");
        writer
            .write_all(&payload)
            .expect("large PTY paste should complete without WouldBlock");
        drop(writer);

        let status = session.wait().expect("PTY child should finish after EOF");
        assert_eq!(status.exit_code(), 0);
        drop(output);
    }

    #[cfg(unix)]
    #[test]
    fn reports_non_terminal_read_errors_once() {
        use std::{
            io,
            sync::{Arc, atomic::AtomicBool, mpsc},
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
        super::read_loop_unix_unpolled(
            &mut FailingReader,
            sender,
            Arc::new(AtomicBool::new(false)),
        );

        assert_eq!(
            receiver
                .recv()
                .expect("reader should report its I/O failure"),
            PtyOutputEvent::Error("simulated PTY read failure".to_owned())
        );
        assert!(
            receiver.try_recv().is_err(),
            "a terminal read failure must end the worker without duplicate events"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reports_eio_as_normal_pty_closure() {
        use std::{
            io,
            sync::{Arc, atomic::AtomicBool, mpsc},
        };

        struct ClosedPtyReader;

        impl io::Read for ClosedPtyReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(nix::errno::Errno::EIO as i32))
            }
        }

        let (sender, receiver) = mpsc::sync_channel(2);
        super::read_loop_unix_unpolled(
            &mut ClosedPtyReader,
            sender,
            Arc::new(AtomicBool::new(false)),
        );

        assert_eq!(
            receiver.recv().expect("reader should report PTY closure"),
            PtyOutputEvent::Closed
        );
        assert!(
            receiver.try_recv().is_err(),
            "normal PTY closure must not emit duplicate events"
        );
    }
}
