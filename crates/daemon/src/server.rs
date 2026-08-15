//! Unix socket server: accept loop and per-connection handling.

use std::{path::Path, sync::Arc, time::Duration};

use std::sync::atomic::{AtomicU64, Ordering};

use agent_bus_core::{DEFAULT_WAIT_TIMEOUT_SECS, MAX_WAIT_TIMEOUT_SECS, PartitionName, Pattern};
use agent_bus_protocol::{Request, Response, encode};
use anyhow::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
    sync::{Mutex, broadcast, mpsc},
};

use crate::{
    handler::{Dispatch, dispatch},
    logging::log_msg,
    state::BusState,
};

/// Serve until a stop request or idle shutdown.
///
/// # Errors
/// Returns an error if the socket cannot be bound.
pub async fn serve(socket: &Path, state: Arc<Mutex<BusState>>) -> Result<()> {
    // A stale socket from a crashed daemon would block bind; the caller holds
    // the exclusive lock, so removing it here is safe.
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;

    // Parked connections (wait/follow) live outside the state mutex, so their
    // counts cannot live in `BusState`; these atomics are bumped on entry to
    // the parked loop and decremented on exit by the RAII guards. Created
    // before the sweep so the sweep can refuse to shut the daemon down while
    // clients are still parked in a wait.
    let active_waiters = Arc::new(AtomicU64::new(0));
    let active_followers = Arc::new(AtomicU64::new(0));

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(4);
    tokio::spawn(crate::sweep::run(
        Arc::clone(&state),
        shutdown_tx.clone(),
        Arc::clone(&active_waiters),
        Arc::clone(&active_followers),
    ));

    // Built once, outside the loop. `ctrl_c()` returns a fresh future each call,
    // and a `select!` arm that rebuilds it every iteration re-registers the
    // handler on each pass; pinning one future keeps a single registration and
    // means a signal that arrives while another arm is running is still seen.
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&state);
                        let shutdown = shutdown_tx.clone();
                        let socket_path = socket.to_path_buf();
                        let aw = Arc::clone(&active_waiters);
                        let af = Arc::clone(&active_followers);
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_connection(stream, state, shutdown, socket_path, aw, af).await
                            {
                                log_msg(&format!("connection error: {e:#}"));
                            }
                        });
                    }
                    Err(e) => log_msg(&format!("accept failed: {e}")),
                }
            }
            _ = shutdown_rx.recv() => {
                log_msg("shutdown requested");
                break;
            }
            _ = &mut interrupt => {
                log_msg("interrupted (SIGINT/SIGTERM)");
                break;
            }
        }
    }

    // Unlink before returning so the next daemon binds a clean path. Dropping
    // the listener alone leaves the inode behind.
    let _ = std::fs::remove_file(socket);
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    state: Arc<Mutex<BusState>>,
    shutdown: mpsc::Sender<()>,
    socket_path: std::path::PathBuf,
    active_waiters: Arc<AtomicU64>,
    active_followers: Arc<AtomicU64>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let request: Request = match agent_bus_protocol::decode(&line) {
            Ok(r) => r,
            Err(e) => {
                let response = Response::Error { message: format!("malformed request: {e}") };
                send(&mut write_half, &response).await?;
                continue;
            }
        };

        // Scoped so the guard is released before any `.await` below: holding it
        // across a client write would let one slow reader stall every publisher.
        let outcome = {
            let mut guard = state.lock().await;
            dispatch(&mut guard, request, &active_waiters, &active_followers)
        };

        match outcome {
            Dispatch::Reply(mut response) => {
                if let Response::Status { status } = &mut response {
                    status.socket_path = socket_path.display().to_string();
                }
                send(&mut write_half, &response).await?;
            }

            Dispatch::Shutdown => {
                send(&mut write_half, &Response::Ok).await?;
                log_msg("stop requested by a client");
                let _ = shutdown.send(()).await;
                return Ok(());
            }

            Dispatch::WaitPending { partition, pattern, label, timeout_secs } => {
                // Clamped rather than rejected: an over-long timeout is a
                // request to wait "as long as possible", and the ceiling is
                // what that actually means here.
                let timeout = Duration::from_secs(
                    timeout_secs.unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS).min(MAX_WAIT_TIMEOUT_SECS),
                );
                let response = wait_for_message(
                    &state,
                    &partition,
                    &pattern,
                    &label,
                    timeout,
                    Arc::clone(&active_waiters),
                )
                .await;
                send(&mut write_half, &response).await?;
            }

            Dispatch::FollowPending { partition, pattern, label } => {
                // Streams until the client goes away. A stream that breaks must
                // be reported to the client before the connection closes:
                // previously the error propagated out of here to the daemon's
                // stderr, which is /dev/null for an auto-started daemon, and
                // the client saw a bare EOF and reported success.
                if let Err(e) = follow(
                    &state,
                    &partition,
                    &pattern,
                    &label,
                    &mut write_half,
                    Arc::clone(&active_followers),
                )
                .await
                {
                    let response = Response::Error { message: format!("follow failed: {e:#}") };
                    // Best-effort: if the write also fails the client is
                    // already gone, which is the ordinary way a follow ends.
                    let _ = send(&mut write_half, &response).await;
                    return Err(e);
                }
                return Ok(());
            }
        }
    }

    Ok(())
}

/// RAII guard that bumps a counter on creation and decrements it on drop.
///
/// The parked loops in `wait` and `follow` have many return paths (messages,
/// timeout, partition errors, client disconnect), and forgetting one would
/// leave a phantom "active" connection. A `Drop` guard makes the decrement
/// unconditional, the same way a lock guard does.
struct ActiveGuard {
    counter: Arc<AtomicU64>,
}

impl ActiveGuard {
    fn new(counter: Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Block until a matching unread message exists, or the timeout expires.
///
/// Re-checks the log on every notification rather than trusting the payload,
/// so a dropped or lagged broadcast cannot lose a message.
///
/// Delivery is exclusive and advances the pattern's position right here: the
/// message handed back is marked delivered for the whole pattern before this
/// returns, so no concurrent consumer can also receive it.
async fn wait_for_message(
    state: &Arc<Mutex<BusState>>,
    partition: &str,
    pattern: &Pattern,
    label: &str,
    timeout: Duration,
    active_waiters: Arc<AtomicU64>,
) -> Response {
    let _waiter_guard = ActiveGuard::new(active_waiters);

    let name = match PartitionName::parse(partition) {
        Ok(n) => n,
        Err(e) => return Response::Error { message: e.to_string() },
    };

    // Subscribed before the first log check, so a publish racing this call
    // either lands in the check below or leaves a pending wake-up here. The
    // reverse order has a gap in which both miss.
    let mut notifications = {
        let mut guard = state.lock().await;
        match guard.partition_mut(&name) {
            Ok(p) => p.subscribe_notifications(),
            Err(e) => return Response::Error { message: e.to_string() },
        }
    };

    // `checked_add` rather than `+`: the caller clamps, but this function is
    // also reachable from tests and any future caller, and a panic here kills
    // the whole connection task rather than failing one request. Saturating to
    // the clamp keeps an absurd duration behaving like the longest legitimate
    // wait instead of aborting the connection.
    let now = tokio::time::Instant::now();
    let deadline = now
        .checked_add(timeout)
        .unwrap_or_else(|| now + Duration::from_secs(MAX_WAIT_TIMEOUT_SECS));

    loop {
        // Check before waiting: a message may have landed between dispatch and
        // subscribing above. The guard is confined to this block and the
        // message cloned out of it, so nothing borrowed from the state crosses
        // the await below.
        let found = {
            let mut guard = state.lock().await;
            match guard.partition_mut(&name) {
                Ok(p) => p.deliver(pattern, label, 1).ok().and_then(|b| b.into_iter().next()),
                Err(e) => return Response::Error { message: e.to_string() },
            }
        };
        if let Some(message) = found {
            return Response::Messages { messages: vec![message] };
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Response::Timeout;
        }

        match tokio::time::timeout(remaining, notifications.recv()).await {
            // Woken by a publish, or lagged behind: either way, re-check.
            Ok(Ok(()) | Err(broadcast::error::RecvError::Lagged(_))) => {}
            // Sender gone: the partition was dropped, nothing more will arrive.
            Ok(Err(broadcast::error::RecvError::Closed)) => return Response::Timeout,
            Err(_elapsed) => return Response::Timeout,
        }
    }
}

/// Stream messages until the client disconnects.
///
/// The loop drains the log before it ever blocks on a notification, so a
/// `follow` that starts after a post still receives that post — the same
/// post-before-subscribe guarantee `wait` gives.
async fn follow(
    state: &Arc<Mutex<BusState>>,
    partition: &str,
    pattern: &Pattern,
    label: &str,
    write_half: &mut OwnedWriteHalf,
    active_followers: Arc<AtomicU64>,
) -> Result<()> {
    let _follower_guard = ActiveGuard::new(active_followers);

    let name = PartitionName::parse(partition)?;
    let mut notifications = {
        let mut guard = state.lock().await;
        guard.partition_mut(&name)?.subscribe_notifications()
    };

    loop {
        // Cloned out of the guard: `deliver` borrows from the state, and those
        // borrows cannot survive the `send` await below.
        let batch = {
            let mut guard = state.lock().await;
            guard.partition_mut(&name)?.deliver(pattern, label, usize::MAX)?
        };

        if !batch.is_empty() {
            // The state guard is released before the send below; the latency
            // is recorded now, under the lock, so a slow reader does not skew
            // "time to pick up" with how long the socket write took.
            {
                let mut guard = state.lock().await;
                for m in &batch {
                    guard.record_delivery_latency(m.id.timestamp_ms());
                }
            }
            send(write_half, &Response::Messages { messages: batch }).await?;
            // Delivery already advanced the pattern's position under the lock,
            // before the bytes went out. A client that dies mid-write cannot be
            // handed the batch again — exclusive delivery means a message is
            // delivered exactly once, not at-least-once.
            continue;
        }

        match notifications.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn send(write_half: &mut OwnedWriteHalf, response: &Response) -> Result<()> {
    let line = encode(response).context("encoding response")?;
    write_half.write_all(line.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_bus_core::{Message, Pattern, Priority, Topic};
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use super::*;

    fn msg(topic: &str, body: &str) -> Message {
        Message::new(Topic::parse(topic).unwrap(), body.to_owned(), Priority::Normal, None)
    }

    fn state(dir: &TempDir) -> Arc<Mutex<BusState>> {
        Arc::new(Mutex::new(BusState::new(dir.path().to_path_buf())))
    }

    fn pattern() -> Pattern {
        Pattern::parse("iot_base/**").unwrap()
    }

    fn name(s: &str) -> PartitionName {
        PartitionName::parse(s).unwrap()
    }

    /// The post-before-subscribe race: the message is already on disk when the
    /// wait begins, so it must come back without any publish to wake it.
    #[tokio::test]
    async fn wait_returns_an_already_stored_message() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);
        state
            .lock()
            .await
            .partition_mut(&name("iot_base"))
            .unwrap()
            .publish(msg("iot_base/dev_01", "hi"))
            .unwrap();

        let response = wait_for_message(
            &state,
            "iot_base",
            &pattern(),
            "reviewer",
            Duration::from_secs(30),
            Arc::new(AtomicU64::new(0)),
        )
        .await;

        match response {
            Response::Messages { messages } => {
                assert_eq!(messages.len(), 1, "wait delivers exactly one message");
                assert_eq!(messages[0].body, "hi");
            }
            other => panic!("expected messages, got {other:?}"),
        }
    }

    /// `wait` is exclusive: the delivered message is marked consumed for the
    /// pattern immediately, so a second wait for the same pattern finds nothing.
    #[tokio::test]
    async fn wait_delivers_a_message_exactly_once() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);
        state
            .lock()
            .await
            .partition_mut(&name("iot_base"))
            .unwrap()
            .publish(msg("iot_base/dev_01", "hi"))
            .unwrap();

        let _ = wait_for_message(
            &state,
            "iot_base",
            &pattern(),
            "reviewer",
            Duration::from_secs(30),
            Arc::new(AtomicU64::new(0)),
        )
        .await;

        let response = wait_for_message(
            &state,
            "iot_base",
            &pattern(),
            "other",
            Duration::ZERO,
            Arc::new(AtomicU64::new(0)),
        )
        .await;
        assert!(
            matches!(response, Response::Timeout),
            "the delivered message must not be handed to any second consumer"
        );
    }

    #[tokio::test]
    async fn wait_times_out_cleanly_when_nothing_arrives() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);

        let response = wait_for_message(
            &state,
            "iot_base",
            &pattern(),
            "reviewer",
            Duration::ZERO,
            Arc::new(AtomicU64::new(0)),
        )
        .await;

        assert!(matches!(response, Response::Timeout), "an empty log must time out, not hang");
    }

    /// Also the deadlock regression test: the publisher can only take the state
    /// lock if the waiter is not holding it while blocked on its notification.
    ///
    /// Multi-threaded on purpose. On the default current-thread runtime the
    /// spawned waiter cannot run while this task holds the lock, so it would
    /// only ever reach its log check *after* the publish and the broadcast
    /// wake-up would never be exercised.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_wakes_on_a_later_publish() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);
        // Open the partition up front so the waiter and the publisher share one
        // broadcast channel rather than racing to create it.
        state.lock().await.partition_mut(&name("iot_base")).unwrap();

        let waiter = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                wait_for_message(
                    &state,
                    "iot_base",
                    &Pattern::parse("iot_base/**").unwrap(),
                    "reviewer",
                    Duration::from_secs(30),
                    Arc::new(AtomicU64::new(0)),
                )
                .await
            })
        };

        // Give the waiter time to find an empty log and park on its
        // notification. A generous margin against a 30s wait timeout: too early
        // and the test still passes via the log re-check, just without proving
        // the wake-up, so this only trades away strictness, never stability.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Taking the lock here is the deadlock check: a waiter that held the
        // state lock while parked would block this forever and the test would
        // hang rather than quietly pass.
        state
            .lock()
            .await
            .partition_mut(&name("iot_base"))
            .unwrap()
            .publish(msg("iot_base/dev_01", "late"))
            .unwrap();

        let response = waiter.await.unwrap();
        match response {
            Response::Messages { messages } => assert_eq!(messages[0].body, "late"),
            other => panic!("expected messages, got {other:?}"),
        }
    }
}
