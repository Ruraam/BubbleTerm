//! Integration coverage for PTY output during externally delivered signals.

#![cfg(unix)]

use bubbleterm_pty::{LaunchSpec, PtyOutputEvent, PtyOutputReader, PtySession, ShellKind};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use std::{
    fs,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn test_size() -> bubbleterm_pty::PtySize {
    bubbleterm_pty::PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[test]
fn sigterm_during_continuous_output_drains_data_before_one_closed_event() {
    const MARKER: &[u8] = b"SIGTERM-ACK";
    const DEADLINE: Duration = Duration::from_secs(5);

    let pid_file = std::env::temp_dir().join(format!(
        "bubbleterm-sigterm-{}-{}.pid",
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
            "trap 'printf '\''SIGTERM-ACK\\n'\''; exit 0' TERM; printf '%s\\n' \"$$\" > \"$BUBBLETERM_TEST_PID_FILE\"; while :; do printf '%4096s' x; done".into(),
        ],
        working_directory: None,
        environment: [(
            std::ffi::OsString::from("BUBBLETERM_TEST_PID_FILE"),
            pid_file.as_os_str().to_owned(),
        )]
        .into_iter()
        .collect(),
        kind: ShellKind::Host,
    };

    let session = PtySession::spawn(&spec, test_size()).expect("PTY process should spawn");
    let output = PtyOutputReader::spawn(&session).expect("PTY output reader should start");

    let consumer = std::thread::spawn(move || {
        let deadline = Instant::now() + DEADLINE;
        let mut received = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "PTY output did not close after SIGTERM"
            );
            match output
                .recv_timeout(remaining)
                .expect("PTY output timed out during SIGTERM teardown")
            {
                PtyOutputEvent::Data(chunk) => received.extend_from_slice(&chunk),
                PtyOutputEvent::Closed => break,
                PtyOutputEvent::Error(error) => {
                    panic!("PTY reader failed during SIGTERM teardown: {error}")
                }
            }
        }

        assert_eq!(
            output.recv_timeout(Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
            "no duplicate or trailing event may follow Closed"
        );
        received
    });

    let pid_deadline = Instant::now() + Duration::from_secs(3);
    let child_pid = loop {
        if let Ok(contents) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                break pid;
            }
        }
        assert!(
            Instant::now() < pid_deadline,
            "PTY child did not publish its PID"
        );
        std::thread::sleep(Duration::from_millis(5));
    };

    // The consumer remains active while the shell continuously writes, so
    // the terminal marker cannot be hidden behind a full bounded channel.
    std::thread::sleep(Duration::from_millis(50));
    signal::kill(Pid::from_raw(child_pid), Signal::SIGTERM)
        .expect("SIGTERM should be delivered to the PTY child");

    let received = consumer
        .join()
        .expect("PTY output consumer should finish after SIGTERM");
    assert!(
        received.len() >= 16 * 1024,
        "the test must exercise a substantial output stream before termination"
    );
    assert!(
        received
            .windows(MARKER.len())
            .any(|window| window == MARKER),
        "the child's SIGTERM handler output must arrive before PTY closure"
    );

    drop(session);
    let _ = fs::remove_file(pid_file);
}
