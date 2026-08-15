//! Integration (I-16): parked waiters defer the idle shutdown, and the daemon
//! exits once they leave. Drives the real binary over the real Unix socket
//! with a short idle/sweep override instead of relying only on the pure
//! `should_shutdown` predicate.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent_bus_protocol::{Request, encode};

/// Spawn the daemon with a 2s idle threshold and a 1s sweep (fast test).
fn spawn_daemon(state_dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_agent-bus-daemon"))
        .env("AGENT_BUS_STATE_DIR", state_dir)
        .env("AGENT_BUS_IDLE_SHUTDOWN_SECS", "2")
        .env("AGENT_BUS_SWEEP_INTERVAL_SECS", "1")
        // The child must not hold the test harness's stdout open — that
        // would make cargo wait forever for the pipe to close.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent-bus-daemon")
}

fn wait_for_socket(state_dir: &Path) {
    let path = state_dir.join("agent-bus.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon socket never appeared at {}", path.display());
}

#[test]
fn a_parked_waiter_defers_idle_shutdown_until_it_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(dir.path());
    wait_for_socket(dir.path());

    // Park a waiter with a 10s timeout — it counts as active use. (The server
    // is parked in the notification wait and only notices a closed socket at
    // wait completion, so "leaving" = the wait timing out.)
    let socket_path = dir.path().join("agent-bus.sock");
    let mut stream = UnixStream::connect(&socket_path).expect("connect");
    let req = Request::Wait {
        pattern: "shutdown_test/**".to_owned(),
        label: "shutdown_test".to_owned(),
        timeout_secs: Some(10),
    };
    stream
        .write_all(format!("{}\n", encode(&req).expect("encode wait")).as_bytes())
        .expect("write wait");

    // The daemon is idle (no posts) but has a parked waiter: it must NOT
    // shut down past the 2s idle threshold plus margin.
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "a parked waiter must keep the daemon alive past the idle threshold"
    );

    // When the wait times out (~10s), the waiter counter returns to zero and
    // the next sweep shuts the daemon down cleanly (exit 0, not a crash).
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "idle shutdown must exit 0, got {status}");
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not shut down after the waiter timed out");
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
}
