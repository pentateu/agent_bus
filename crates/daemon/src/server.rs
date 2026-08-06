//! Unix socket server: accept loop and per-connection handling.

use std::{path::Path, sync::Arc, time::Duration};

use agent_bus_core::{Message, Pattern};
use agent_bus_protocol::{Request, Response, encode};
use anyhow::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
    sync::{Mutex, broadcast, mpsc},
};

use crate::{
    handler::{Dispatch, dispatch},
    state::BusState,
};

/// Default `wait` timeout when the client does not supply one: 30 minutes.
/// Bounded so a client never blocks forever against a harness tool timeout.
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 1800;

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

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(4);
    tokio::spawn(crate::sweep::run(Arc::clone(&state), shutdown_tx.clone()));

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
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_connection(stream, state, shutdown, socket_path).await
                            {
                                eprintln!("agent-bus: connection error: {e:#}");
                            }
                        });
                    }
                    Err(e) => eprintln!("agent-bus: accept failed: {e}"),
                }
            }
            _ = shutdown_rx.recv() => break,
            _ = &mut interrupt => break,
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
            dispatch(&mut guard, request)
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
                let _ = shutdown.send(()).await;
                return Ok(());
            }

            Dispatch::WaitPending { partition, pattern, subscriber, timeout_secs } => {
                let timeout =
                    Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS));
                let response =
                    wait_for_message(&state, &partition, &pattern, &subscriber, timeout).await;
                send(&mut write_half, &response).await?;
            }

            Dispatch::FollowPending { partition, pattern, subscriber } => {
                // Streams until the client goes away; errors end the loop.
                follow(&state, &partition, &pattern, &subscriber, &mut write_half).await?;
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Block until a matching unread message exists, or the timeout expires.
///
/// Re-checks the log on every notification rather than trusting the payload,
/// so a dropped or lagged broadcast cannot lose a message.
///
/// Deliberately does not acknowledge: `wait` is at-least-once, and the cursor
/// only moves when the client sends an explicit `Ack` after it has printed the
/// message. A client killed between delivery and ack re-reads it next time.
async fn wait_for_message(
    state: &Arc<Mutex<BusState>>,
    partition: &str,
    pattern: &Pattern,
    subscriber: &str,
    timeout: Duration,
) -> Response {
    // Subscribed before the first log check, so a publish racing this call
    // either lands in the check below or leaves a pending wake-up here. The
    // reverse order has a gap in which both miss.
    let mut notifications = {
        let mut guard = state.lock().await;
        match guard.partition_mut(partition) {
            Ok(p) => p.subscribe_notifications(),
            Err(e) => return Response::Error { message: e.to_string() },
        }
    };

    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        // Check before waiting: a message may have landed between dispatch and
        // subscribing above. The guard is confined to this block and the
        // message cloned out of it, so nothing borrowed from the state crosses
        // the await below.
        let found = {
            let mut guard = state.lock().await;
            match guard.partition_mut(partition) {
                Ok(p) => p.unread(pattern, subscriber).first().map(|m| (*m).clone()),
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
    subscriber: &str,
    write_half: &mut OwnedWriteHalf,
) -> Result<()> {
    let mut notifications = {
        let mut guard = state.lock().await;
        guard.partition_mut(partition)?.subscribe_notifications()
    };

    loop {
        // Cloned out of the guard: `unread` borrows from the state, and those
        // borrows cannot survive the `send` await below.
        let batch: Vec<Message> = {
            let mut guard = state.lock().await;
            guard
                .partition_mut(partition)?
                .unread(pattern, subscriber)
                .into_iter()
                .cloned()
                .collect()
        };

        if let Some(last) = batch.last().map(|m| m.id) {
            send(write_half, &Response::Messages { messages: batch }).await?;
            // Advance only after the bytes are out, so a client that dies
            // mid-write re-reads rather than silently losing messages.
            let mut guard = state.lock().await;
            guard.partition_mut(partition)?.acknowledge(subscriber, last)?;
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

    /// The post-before-subscribe race: the message is already on disk when the
    /// wait begins, so it must come back without any publish to wake it.
    #[tokio::test]
    async fn wait_returns_an_already_stored_message() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);
        state
            .lock()
            .await
            .partition_mut("iot_base")
            .unwrap()
            .publish(msg("iot_base/dev_01", "hi"))
            .unwrap();

        let response =
            wait_for_message(&state, "iot_base", &pattern(), "reviewer", Duration::from_secs(30))
                .await;

        match response {
            Response::Messages { messages } => {
                assert_eq!(messages.len(), 1, "wait delivers exactly one message");
                assert_eq!(messages[0].body, "hi");
            }
            other => panic!("expected messages, got {other:?}"),
        }
    }

    /// `wait` is at-least-once: the cursor must stay put until the client acks.
    #[tokio::test]
    async fn wait_does_not_advance_the_cursor() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);
        state
            .lock()
            .await
            .partition_mut("iot_base")
            .unwrap()
            .publish(msg("iot_base/dev_01", "hi"))
            .unwrap();

        let _ =
            wait_for_message(&state, "iot_base", &pattern(), "reviewer", Duration::from_secs(30))
                .await;

        let mut guard = state.lock().await;
        let unread = guard.partition_mut("iot_base").unwrap().unread(&pattern(), "reviewer").len();
        assert_eq!(unread, 1, "wait must not acknowledge; the CLI acks explicitly");
    }

    #[tokio::test]
    async fn wait_times_out_cleanly_when_nothing_arrives() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);

        let response =
            wait_for_message(&state, "iot_base", &pattern(), "reviewer", Duration::ZERO).await;

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
        state.lock().await.partition_mut("iot_base").unwrap();

        let waiter = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                wait_for_message(
                    &state,
                    "iot_base",
                    &Pattern::parse("iot_base/**").unwrap(),
                    "reviewer",
                    Duration::from_secs(30),
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
            .partition_mut("iot_base")
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
