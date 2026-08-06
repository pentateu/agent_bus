//! End-to-end tests over a real daemon and a real Unix socket.
//!
//! Each test gets its own state directory, so a stray daemon from one test
//! cannot affect another. That isolation is what lets these run in parallel:
//! the daemon keys everything — socket, lock file, logs, cursors — off
//! `AGENT_BUS_STATE_DIR`, so two tests never share a partition or a process.

// An integration test is its own crate, so the library's
// `cfg_attr(test, allow(clippy::unwrap_used))` does not reach it. Panicking on
// a broken assumption is the correct behaviour for a test: it is a failure
// report, not an unhandled error path.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use tempfile::TempDir;

/// Path to a built binary in the same profile as the test runner.
///
/// Integration test binaries are placed in `target/<profile>/deps`, while the
/// crate's own binaries land in `target/<profile>`, so one `pop` off `deps`
/// reaches them. The `ends_with` check keeps this correct under both layouts.
fn binary(name: &str) -> PathBuf {
    let mut dir = std::env::current_exe().expect("test executable path");
    dir.pop(); // the test binary's own filename
    if dir.ends_with("deps") {
        dir.pop();
    }
    let path = dir.join(name);
    assert!(path.exists(), "{} is not built; run `cargo build` first", path.display());
    path
}

/// Build an `agent-bus` command bound to an isolated state directory.
fn command(state: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(binary("agent-bus"));
    cmd.args(args)
        .env("AGENT_BUS_STATE_DIR", state)
        .env("AGENT_BUS_DAEMON_BIN", binary("agent-bus-daemon"));
    cmd
}

/// Run `agent-bus` against an isolated state directory.
fn bus(state: &Path, args: &[&str]) -> Output {
    command(state, args).output().expect("running agent-bus")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

/// Assert a command succeeded, surfacing stderr when it did not.
fn assert_ok(output: &Output, what: &str) {
    assert_eq!(code(output), 0, "{what} failed: {}", stderr(output));
}

/// Parse `--json` message output, one JSON object per line.
fn messages(output: &Output) -> Vec<serde_json::Value> {
    stdout(output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each message line must be valid JSON"))
        .collect()
}

/// The `body` field of every message in a `--json` listing.
fn bodies(output: &Output) -> Vec<String> {
    messages(output)
        .iter()
        .map(|m| m["body"].as_str().expect("body must be a string").to_owned())
        .collect()
}

/// Shut the daemon down and wait for it to actually let go.
///
/// The daemon unlinks its socket on the way out, so the socket disappearing is
/// a real signal that the process has finished rather than a guess. Polling for
/// that beats a blind sleep: a daemon still alive when `TempDir` drops would be
/// writing into a deleted directory.
fn stop(state: &Path) {
    let output = bus(state, &["stop"]);
    assert_eq!(code(&output), 0, "stop must succeed even with no daemon: {}", stderr(&output));

    let socket = state.join("agent-bus.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists(), "the daemon did not remove its socket within 10s");
}

#[test]
fn post_then_read_delivers_the_message() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    let posted = bus(state, &["post", "iot_base/dev_01", "ready for review"]);
    assert_eq!(code(&posted), 0, "post failed: {}", stderr(&posted));

    let read = bus(state, &["read", "iot_base/**", "--as", "reviewer", "--json"]);
    assert_eq!(code(&read), 0);
    assert!(stdout(&read).contains("ready for review"));

    stop(state);
}

#[test]
fn the_daemon_auto_starts_on_first_use() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    // No daemon has been started explicitly; post must still succeed.
    let posted = bus(state, &["post", "iot_base/dev_01", "hello"]);
    assert_eq!(code(&posted), 0, "auto-start failed");
    assert!(state.join("agent-bus.sock").exists(), "the socket must exist after auto-start");

    stop(state);
}

#[test]
fn wait_returns_a_message_posted_before_it_started() {
    // The core race: the reviewer connects after the dev has already posted.
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "posted first"]);

    let waited = bus(state, &["wait", "iot_base/**", "--as", "late_reviewer", "--timeout", "5s"]);
    assert_eq!(code(&waited), 0, "wait must not block on an already-stored message");
    assert!(stdout(&waited).contains("posted first"));

    stop(state);
}

#[test]
fn wait_times_out_with_exit_code_two() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["daemon", "iot_base/**"]);
    let waited = bus(state, &["wait", "iot_base/**", "--as", "nobody", "--timeout", "1s"]);
    assert_eq!(code(&waited), 2, "a timeout must exit 2 so shell loops terminate");

    stop(state);
}

#[test]
fn reading_twice_does_not_redeliver() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "only once"]);

    let first = bus(state, &["read", "iot_base/**", "--as", "reviewer"]);
    assert!(stdout(&first).contains("only once"));

    let second = bus(state, &["read", "iot_base/**", "--as", "reviewer"]);
    assert!(!stdout(&second).contains("only once"), "the cursor must suppress redelivery");

    stop(state);
}

#[test]
fn subscribers_have_independent_cursors() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "shared message"]);
    bus(state, &["read", "iot_base/**", "--as", "reviewer"]);

    let planner = bus(state, &["read", "iot_base/**", "--as", "planner"]);
    assert!(
        stdout(&planner).contains("shared message"),
        "one subscriber reading must not consume for another"
    );

    stop(state);
}

#[test]
fn cursors_survive_a_daemon_restart() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "before restart"]);
    bus(state, &["read", "iot_base/**", "--as", "reviewer"]);
    stop(state);

    // Next command auto-starts a fresh daemon.
    let after = bus(state, &["read", "iot_base/**", "--as", "reviewer"]);
    assert!(
        !stdout(&after).contains("before restart"),
        "a restarted daemon must reload cursors, not replay consumed messages"
    );

    stop(state);
}

#[test]
fn partitions_are_isolated() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "project one secret"]);
    bus(state, &["post", "other_proj/dev_01", "project two secret"]);

    let iot = bus(state, &["read", "iot_base/**", "--as", "r1"]);
    assert!(stdout(&iot).contains("project one secret"));
    assert!(
        !stdout(&iot).contains("project two secret"),
        "partitions must not leak into each other"
    );

    stop(state);
}

#[test]
fn wildcards_respect_segment_depth() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "one level"]);
    bus(state, &["post", "iot_base/team/dev_02", "two levels"]);

    let single = bus(state, &["read", "iot_base/*", "--as", "single"]);
    assert!(stdout(&single).contains("one level"));
    assert!(!stdout(&single).contains("two levels"), "* must match exactly one segment");

    let deep = bus(state, &["read", "iot_base/**", "--as", "deep"]);
    assert!(stdout(&deep).contains("two levels"), "** must match any depth");

    stop(state);
}

#[test]
fn history_replays_consumed_messages() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "already read"]);
    bus(state, &["read", "iot_base/**", "--as", "reviewer"]);

    let history = bus(state, &["history", "iot_base/**"]);
    assert!(
        stdout(&history).contains("already read"),
        "history ignores cursors so a restarted agent can rebuild context"
    );

    stop(state);
}

#[test]
fn history_without_since_returns_everything_retained() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/a", "first"]);
    bus(state, &["post", "iot_base/b", "second"]);

    let history = bus(state, &["history", "iot_base/**"]);
    let out = stdout(&history);
    assert!(out.contains("first") && out.contains("second"), "bare history must not truncate");

    stop(state);
}

#[test]
fn daemon_command_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    let first = bus(state, &["daemon", "iot_base/*"]);
    assert_eq!(code(&first), 0);
    assert!(stdout(&first).contains("ready"));

    // A second call, and an overlapping narrower pattern, must both be no-ops.
    let second = bus(state, &["daemon", "iot_base/*"]);
    assert!(stdout(&second).contains("already running"));
    let narrower = bus(state, &["daemon", "iot_base/dev_01"]);
    assert!(
        stdout(&narrower).contains("already running"),
        "an overlapping pattern shares the partition rather than creating a second one"
    );

    stop(state);
}

#[test]
fn concurrent_first_use_starts_exactly_one_daemon() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().to_path_buf();

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let state = state.clone();
            std::thread::spawn(move || {
                let body = format!("message {i}");
                let out = Command::new(binary("agent-bus"))
                    .args(["post", "iot_base/dev_01", &body])
                    .env("AGENT_BUS_STATE_DIR", &state)
                    .env("AGENT_BUS_DAEMON_BIN", binary("agent-bus-daemon"))
                    .output()
                    .expect("running agent-bus");
                out.status.code().unwrap_or(-1)
            })
        })
        .collect();

    for handle in handles {
        assert_eq!(handle.join().unwrap(), 0, "every concurrent post must succeed");
    }

    // All eight landed in one log, proving they reached the same daemon. The
    // bodies are parsed out of the JSON rather than counted as substrings, so a
    // body appearing inside some other field could not inflate the count.
    let history = bus(&state, &["history", "iot_base/**", "--json"]);
    let mut found = bodies(&history);
    found.sort();
    let expected: Vec<String> = (0..8).map(|i| format!("message {i}")).collect();
    assert_eq!(found, expected, "all posts must reach a single shared daemon");

    // Exactly one daemon owns the state dir, so one pid answers for all of them.
    let status = bus(&state, &["status", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(
        parsed["partitions"].as_array().map(Vec::len),
        Some(1),
        "the eight posts must share a single partition in a single daemon"
    );

    stop(&state);
}

#[test]
fn invalid_input_exits_one() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    // A wildcard in the partition segment would break isolation.
    let bad_pattern = bus(state, &["read", "*/dev_01", "--as", "x"]);
    assert_eq!(code(&bad_pattern), 1);

    // Posting to a wildcard is meaningless.
    let bad_topic = bus(state, &["post", "iot_base/*", "body"]);
    assert_eq!(code(&bad_topic), 1);

    stop(state);
}

/// `stop` must never resurrect a daemon in order to kill it: a state directory
/// that has never seen one stays empty of a socket afterwards.
#[test]
fn stop_without_a_daemon_succeeds_and_starts_nothing() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    let stopped = bus(state, &["stop"]);
    assert_eq!(code(&stopped), 0, "stopping an absent daemon is not an error");
    assert!(
        !state.join("agent-bus.sock").exists(),
        "stop must not auto-start a daemon just to shut it down"
    );
}

#[test]
fn wait_wakes_when_a_message_arrives_later() {
    const BLOCK_FOR: Duration = Duration::from_millis(500);

    let dir = TempDir::new().unwrap();
    let state = dir.path().to_path_buf();

    bus(&state, &["daemon", "iot_base/**"]);

    let waiter_state = state.clone();
    let started = Instant::now();
    let waiter = std::thread::spawn(move || {
        Command::new(binary("agent-bus"))
            .args(["wait", "iot_base/**", "--as", "reviewer", "--timeout", "20s"])
            .env("AGENT_BUS_STATE_DIR", &waiter_state)
            .env("AGENT_BUS_DAEMON_BIN", binary("agent-bus-daemon"))
            .output()
            .expect("running wait")
    });

    // Give the waiter time to block before posting. If the post somehow wins
    // the race, the daemon's pre-check still delivers the message, so this can
    // never flake — it would only stop proving the wake-up path, which the
    // elapsed-time assertion below then catches.
    std::thread::sleep(BLOCK_FOR);
    bus(&state, &["post", "iot_base/dev_01", "arrived late"]);

    let output = waiter.join().unwrap();
    let elapsed = started.elapsed();
    assert_eq!(code(&output), 0, "wait must wake on publish");
    assert!(stdout(&output).contains("arrived late"));

    // The waiter cannot have returned before the post existed, so it genuinely
    // blocked and was woken rather than being answered from the stored log.
    assert!(
        elapsed >= BLOCK_FOR,
        "wait returned in {elapsed:?}, before the message was posted: it did not block"
    );

    stop(&state);
}

#[test]
fn status_reports_partitions_and_lag() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "unread message"]);
    bus(state, &["read", "iot_base/**", "--as", "reviewer"]);
    bus(state, &["post", "iot_base/dev_01", "second message"]);

    let status = bus(state, &["status", "--json"]);
    assert_eq!(code(&status), 0);
    let out = stdout(&status);

    let parsed: serde_json::Value = serde_json::from_str(&out).expect("status must be valid JSON");
    let partition = parsed["partitions"]
        .as_array()
        .expect("partitions must be an array")
        .iter()
        .find(|p| p["name"] == "iot_base")
        .expect("the iot_base partition must be reported");
    assert_eq!(partition["message_count"].as_u64().unwrap(), 2);

    // Looked up by id rather than by position: subscriber ordering is not part
    // of the contract, so indexing [0] would be asserting on an accident.
    let subscriber = partition["subscribers"]
        .as_array()
        .expect("subscribers must be an array")
        .iter()
        .find(|s| s["id"] == "reviewer")
        .expect("the reviewer subscriber must be reported");
    assert_eq!(subscriber["lag"].as_u64().unwrap(), 1, "one message posted after the last read");

    stop(state);
}

#[test]
fn corrupt_log_lines_do_not_prevent_startup() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "valid message"]);
    stop(state);

    // Simulate a torn write from a crash.
    let log = state.join("iot_base.jsonl");
    let mut contents = std::fs::read_to_string(&log).unwrap();
    contents.push_str("{ this is not valid json\n");
    std::fs::write(&log, contents).unwrap();

    let history = bus(state, &["history", "iot_base/**"]);
    assert_eq!(code(&history), 0, "one bad line must not make the partition unreadable");
    assert!(stdout(&history).contains("valid message"));

    // Parsed rather than substring-matched: the pretty-printer's exact spacing
    // is not part of the contract, the reported count is.
    let status = bus(state, &["status", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    let partition = parsed["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "iot_base")
        .expect("the iot_base partition must be reported");
    assert_eq!(
        partition["skipped_records"].as_u64().unwrap(),
        1,
        "corruption must be reported, not silently swallowed"
    );

    stop(state);
}

/// `follow` streams rather than exiting, so it is driven as a child process
/// whose stdout is read line by line and then killed.
///
/// Both the already-stored message and one published afterwards must arrive,
/// which is what distinguishes streaming from a one-shot read. Reading through
/// a channel with a deadline means a daemon that never sends fails the test
/// instead of hanging the suite.
#[test]
fn follow_streams_stored_and_later_messages() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().to_path_buf();

    bus(&state, &["post", "iot_base/dev_01", "before follow"]);

    let mut child = command(&state, &["follow", "iot_base/**", "--as", "follower", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawning follow");

    let out = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let next = |rx: &mpsc::Receiver<String>| -> serde_json::Value {
        let line = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("follow must stream a message within 20s");
        serde_json::from_str(&line).expect("follow --json emits one JSON object per line")
    };

    // The message that predates the follow: same post-before-subscribe
    // guarantee that `wait` gives.
    assert_eq!(next(&rx)["body"], "before follow");

    bus(&state, &["post", "iot_base/dev_01", "after follow"]);
    assert_eq!(next(&rx)["body"], "after follow");

    child.kill().expect("killing follow");
    child.wait().expect("reaping follow");
    drop(rx);
    let _ = reader.join();

    // The daemon advances the cursor server-side as it streams, so a later
    // reader under the same id sees nothing left over.
    let leftover = bus(&state, &["read", "iot_base/**", "--as", "follower", "--json"]);
    assert!(
        bodies(&leftover).is_empty(),
        "follow advances the cursor server-side: {}",
        stdout(&leftover)
    );

    stop(&state);
}

/// The subscriber id defaults to the pattern string, so two reads with the same
/// pattern and no `--as` share one cursor, while a different pattern does not.
#[test]
fn the_subscriber_id_defaults_to_the_pattern() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "default id message"]);

    let first = bus(state, &["read", "iot_base/**", "--json"]);
    assert_eq!(bodies(&first), vec!["default id message"]);

    // Same pattern, no --as: the same implicit cursor, so nothing is left.
    let again = bus(state, &["read", "iot_base/**", "--json"]);
    assert!(bodies(&again).is_empty(), "the pattern-derived cursor must persist across runs");

    // A different pattern is a different subscriber and still has it unread.
    let other = bus(state, &["read", "iot_base/*", "--json"]);
    assert_eq!(
        bodies(&other),
        vec!["default id message"],
        "a different pattern defaults to a different subscriber id"
    );

    // And the default id is literally the pattern string.
    let status = bus(state, &["status", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    let ids: Vec<&str> = parsed["partitions"][0]["subscribers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&"iot_base/**"), "the default id is the pattern itself: {ids:?}");
    assert!(ids.contains(&"iot_base/*"), "the default id is the pattern itself: {ids:?}");

    stop(state);
}

/// Exit code 3 is the "daemon unreachable" contract agents branch on to decide
/// between retrying and failing. Pointing the spawn at a binary that exits
/// immediately without binding a socket reproduces that without needing a
/// broken daemon: the CLI's start timeout expires and it must report 3, not 1.
///
/// This deliberately costs the CLI's full 5s start timeout.
#[test]
fn an_unreachable_daemon_exits_three() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    let output = Command::new(binary("agent-bus"))
        .args(["post", "iot_base/dev_01", "nobody is listening"])
        .env("AGENT_BUS_STATE_DIR", state)
        .env("AGENT_BUS_DAEMON_BIN", "/usr/bin/true")
        .output()
        .expect("running agent-bus");

    assert_eq!(
        code(&output),
        3,
        "an unreachable daemon must be distinguishable from a usage error: {}",
        stderr(&output)
    );
    assert!(!state.join("agent-bus.sock").exists(), "no socket should have appeared");
}

/// A daemon binary that cannot be executed at all is still "unreachable", not a
/// usage error, and it must fail fast rather than burning the start timeout.
#[test]
fn a_missing_daemon_binary_exits_three() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    let output = Command::new(binary("agent-bus"))
        .args(["post", "iot_base/dev_01", "no daemon binary"])
        .env("AGENT_BUS_STATE_DIR", state)
        .env("AGENT_BUS_DAEMON_BIN", state.join("does-not-exist"))
        .output()
        .expect("running agent-bus");

    assert_eq!(
        code(&output),
        3,
        "a failed spawn is unavailability, not usage: {}",
        stderr(&output)
    );
}

/// `post` with no body reads it from stdin, which is what shell pipelines and
/// harness hooks rely on.
#[test]
fn post_reads_the_body_from_stdin_when_omitted() {
    use std::io::Write;

    let dir = TempDir::new().unwrap();
    let state = dir.path();

    // stdout is piped rather than inherited so the child's "posted <id>" line
    // does not leak into the test harness's own output.
    let mut child = command(state, &["post", "iot_base/dev_01"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawning post");
    child.stdin.take().expect("piped stdin").write_all(b"piped body").expect("writing the body");
    let status = child.wait().expect("reaping post");
    assert_eq!(status.code(), Some(0), "a piped body must post successfully");

    let read = bus(state, &["read", "iot_base/**", "--as", "reader", "--json"]);
    assert_ok(&read, "read");
    assert_eq!(bodies(&read), vec!["piped body"]);

    stop(state);
}

/// `--json` message output must be parseable, one object per line, carrying the
/// documented fields. Agents consume this, so its shape is a contract.
#[test]
fn json_messages_carry_the_documented_fields() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "shaped", "--priority", "high", "--from", "dev_01"]);

    let read = bus(state, &["read", "iot_base/**", "--as", "shape", "--json"]);
    assert_ok(&read, "read");
    let parsed = messages(&read);
    assert_eq!(parsed.len(), 1);
    let message = &parsed[0];

    for field in ["id", "ts", "topic", "priority", "from", "body"] {
        assert!(message.get(field).is_some(), "the {field} field is part of the wire contract");
    }
    assert_eq!(message["topic"], "iot_base/dev_01");
    assert_eq!(message["body"], "shaped");
    assert_eq!(message["priority"], "high");
    assert_eq!(message["from"], "dev_01");

    stop(state);
}

/// `read` drains every unread message at once, unlike `wait`, which returns at
/// most one.
#[test]
fn read_drains_all_unread_but_wait_takes_one() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    for body in ["one", "two", "three"] {
        assert_ok(&bus(state, &["post", "iot_base/dev_01", body]), "post");
    }

    let waited = bus(state, &["wait", "iot_base/**", "--as", "w", "--timeout", "5s", "--json"]);
    assert_ok(&waited, "wait");
    assert_eq!(bodies(&waited), vec!["one"], "wait returns exactly one, oldest first");

    let drained = bus(state, &["read", "iot_base/**", "--as", "w", "--json"]);
    assert_ok(&drained, "read");
    assert_eq!(bodies(&drained), vec!["two", "three"], "read drains the rest");

    let empty = bus(state, &["read", "iot_base/**", "--as", "w", "--json"]);
    assert!(bodies(&empty).is_empty(), "read acks what it printed");

    stop(state);
}

/// A bare partition name matches the partition and everything below it.
#[test]
fn a_bare_partition_name_matches_the_whole_partition() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "one level"]);
    bus(state, &["post", "iot_base/team/dev_02", "two levels"]);

    let all = bus(state, &["read", "iot_base", "--as", "bare", "--json"]);
    assert_ok(&all, "read");
    let mut found = bodies(&all);
    found.sort();
    assert_eq!(found, vec!["one level", "two levels"]);

    stop(state);
}

/// `history --since` windows the replay, and unlike `read` it never moves a
/// cursor, so it can be run repeatedly.
#[test]
fn history_is_repeatable_and_windowed() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    bus(state, &["post", "iot_base/dev_01", "historic"]);

    let first = bus(state, &["history", "iot_base/**", "--json"]);
    let second = bus(state, &["history", "iot_base/**", "--json"]);
    assert_eq!(bodies(&first), vec!["historic"]);
    assert_eq!(bodies(&second), vec!["historic"], "history must not consume what it replays");

    // A window that spans the just-posted message still includes it.
    let recent = bus(state, &["history", "iot_base/**", "--since", "1h", "--json"]);
    assert_eq!(bodies(&recent), vec!["historic"]);

    // An unparseable duration is a usage error, not a silent full replay.
    let bad = bus(state, &["history", "iot_base/**", "--since", "soon"]);
    assert_eq!(code(&bad), 1, "a bad duration must be rejected");

    stop(state);
}

/// `guide` and `status` must work without arguments, and `guide` must not need
/// a daemon at all — it is what an agent runs first to learn the tool.
#[test]
fn guide_prints_without_a_daemon() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    let guide = bus(state, &["guide"]);
    assert_ok(&guide, "guide");
    assert!(!stdout(&guide).is_empty(), "the guide must have content");
    assert!(
        !state.join("agent-bus.sock").exists(),
        "guide is pure output and must not start a daemon"
    );
}

/// `hook install --dry-run` prints configuration without touching the disk.
#[test]
fn hook_install_dry_run_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let state = dir.path();

    let hook = bus(state, &["hook", "install", "claude-code", "iot_base/**", "--dry-run"]);
    assert_ok(&hook, "hook install --dry-run");
    assert!(!stdout(&hook).is_empty(), "a dry run must show what it would write");

    // Rejected by clap's own value parser before the command ever runs, so this
    // is clap's usage exit code (2), not the bus's own `Usage` (1). Asserted as
    // "not success" because which of the two applies is clap's business, and
    // pinning it here would make an argument-parsing detail a bus contract.
    let unknown = bus(state, &["hook", "install", "not-a-harness", "iot_base/**", "--dry-run"]);
    assert_ne!(code(&unknown), 0, "an unknown harness must be rejected");
}
