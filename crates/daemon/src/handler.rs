//! Translates protocol requests into state mutations.
//!
//! Every arm here is a single synchronous state transition. The two requests
//! that cannot be answered immediately — `Wait` with nothing unread, and
//! `Follow` — return a `Dispatch` variant describing what the server must do
//! instead, because blocking needs the connection loop that lives in
//! `server.rs`.

use std::time::{SystemTime, UNIX_EPOCH};

use agent_bus_core::{Message, Pattern, Topic};
use agent_bus_protocol::{PartitionReport, Request, Response, StatusReport, SubscriberReport};
use ulid::Ulid;

use crate::{partition::Partition, state::BusState};

/// Seconds since the Unix epoch.
///
/// A clock before the epoch is not a condition the daemon can act on, so it
/// degrades to 0 rather than failing a request: the only consequence is that
/// age-based retention treats everything as brand new.
#[must_use]
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Outcome of dispatching one request.
pub enum Dispatch {
    /// Send this response and continue.
    Reply(Response),
    /// A `Wait` that found nothing: the server must block, then retry.
    WaitPending {
        partition: String,
        pattern: Pattern,
        subscriber: String,
        timeout_secs: Option<u64>,
    },
    /// A `Follow`: the server streams until the client disconnects.
    FollowPending { partition: String, pattern: Pattern, subscriber: String },
    /// Shut the daemon down after replying.
    Shutdown,
}

/// Handle one request against the daemon's state.
pub fn dispatch(state: &mut BusState, request: Request) -> Dispatch {
    state.touch();
    match request {
        Request::Ensure { pattern } => {
            let parsed = match Pattern::parse(&pattern) {
                Ok(p) => p,
                Err(e) => return Dispatch::Reply(error(&e)),
            };
            let name = parsed.partition().to_owned();
            // Sampled before `partition_mut`, which is what creates it.
            let already_running = state.partition_exists(&name);
            match state.partition_mut(&name) {
                Ok(_) => Dispatch::Reply(Response::Ensured { partition: name, already_running }),
                Err(e) => Dispatch::Reply(error(&e)),
            }
        }

        Request::Post { topic, body, priority, from } => {
            let topic = match Topic::parse(&topic) {
                Ok(t) => t,
                Err(e) => return Dispatch::Reply(error(&e)),
            };
            let name = topic.partition().to_owned();
            let message = Message::new(topic, body, priority, from);
            match state.partition_mut(&name).and_then(|p| p.publish(message)) {
                Ok(id) => Dispatch::Reply(Response::Posted { id: id.to_string() }),
                Err(e) => Dispatch::Reply(error(&e)),
            }
        }

        Request::Wait { pattern, subscriber, timeout_secs } => {
            let (pattern, partition) = match resolve(state, &pattern) {
                Ok(pair) => pair,
                Err(response) => return Dispatch::Reply(response),
            };
            // A matching message may already have been posted before this wait
            // began. Answering from the log here, rather than only listening
            // for the next publish, is what makes the post-then-subscribe race
            // safe.
            if let Some(first) = partition.unread(&pattern, &subscriber).first() {
                Dispatch::Reply(Response::Messages { messages: vec![(*first).clone()] })
            } else {
                Dispatch::WaitPending {
                    partition: partition.name().to_owned(),
                    pattern,
                    subscriber,
                    timeout_secs,
                }
            }
        }

        Request::Read { pattern, subscriber } => match resolve(state, &pattern) {
            Ok((pattern, partition)) => Dispatch::Reply(Response::Messages {
                messages: partition.unread(&pattern, &subscriber).into_iter().cloned().collect(),
            }),
            Err(response) => Dispatch::Reply(response),
        },

        Request::Follow { pattern, subscriber } => match resolve(state, &pattern) {
            Ok((pattern, partition)) => Dispatch::FollowPending {
                partition: partition.name().to_owned(),
                pattern,
                subscriber,
            },
            Err(response) => Dispatch::Reply(response),
        },

        Request::History { pattern, since_secs } => {
            let now = now_secs();
            match resolve(state, &pattern) {
                Ok((pattern, partition)) => Dispatch::Reply(Response::Messages {
                    messages: partition
                        .history(&pattern, since_secs, now)
                        .into_iter()
                        .cloned()
                        .collect(),
                }),
                Err(response) => Dispatch::Reply(response),
            }
        }

        // The client names the partition directly here rather than sending a
        // pattern, but that grants no extra reach: an `ack` only ever moves
        // that subscriber's own cursor forward, and `CursorStore::advance`
        // refuses to move one backwards. The worst a wrong partition can do is
        // skip messages for the naming client itself; no other subscriber's
        // delivery and no other partition's contents are affected.
        Request::Ack { partition, subscriber, id } => {
            let Ok(id) = id.parse::<Ulid>() else {
                return Dispatch::Reply(Response::Error {
                    message: format!("invalid cursor id {id:?}"),
                });
            };
            match state.partition_mut(&partition).and_then(|p| p.acknowledge(&subscriber, id)) {
                Ok(()) => Dispatch::Reply(Response::Ok),
                Err(e) => Dispatch::Reply(error(&e)),
            }
        }

        Request::Status => Dispatch::Reply(Response::Status { status: build_status(state) }),

        Request::Stop => Dispatch::Shutdown,
    }
}

/// Parse a pattern and open the partition it names.
///
/// Returns both, borrowed together, so the caller does not have to re-look-up
/// the partition by name after parsing.
fn resolve<'s>(
    state: &'s mut BusState,
    pattern: &str,
) -> Result<(Pattern, &'s mut Partition), Response> {
    let parsed = Pattern::parse(pattern).map_err(|e| error(&e))?;
    let partition = state.partition_mut(parsed.partition()).map_err(|e| error(&e))?;
    Ok((parsed, partition))
}

fn error(e: &dyn std::fmt::Display) -> Response {
    Response::Error { message: e.to_string() }
}

/// Build a status snapshot.
///
/// `socket_path` is left empty for the server to fill in: the path is a
/// property of the listener, not of the state this module owns, and threading
/// it through every `dispatch` call purely so one status arm can read it would
/// be worse. `StatusReport` is a wire type in `protocol`, so this module cannot
/// make the field non-optional or move it without changing a crate the task
/// forbids touching.
fn build_status(state: &BusState) -> StatusReport {
    let now = now_secs();
    let partitions = state
        .partitions()
        .map(|p| PartitionReport {
            name: p.name().to_owned(),
            message_count: p.message_count(),
            oldest_age_secs: p.oldest_age_secs(now),
            skipped_records: p.skipped_records(),
            subscribers: p
                .subscriber_snapshots()
                .into_iter()
                .map(|s| SubscriberReport {
                    id: s.id,
                    cursor: s.cursor,
                    lag: s.lag,
                    snapped: s.snapped,
                })
                .collect(),
        })
        .collect();

    StatusReport {
        pid: std::process::id(),
        uptime_secs: state.uptime_secs(),
        socket_path: String::new(), // filled in by the server, which knows the path
        partitions,
    }
}
