//! Translates protocol requests into state mutations.
//!
//! Every arm here is a single synchronous state transition. The two requests
//! that cannot be answered immediately — `Wait` with nothing unread, and
//! `Follow` — return a `Dispatch` variant describing what the server must do
//! instead, because blocking needs the connection loop that lives in
//! `server.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_bus_core::{Message, PartitionName, Pattern, Topic};
use agent_bus_protocol::{
    ConsumerLag, MetricsReport, MetricsTotals, PartitionMetrics, PartitionReport, PatternReport,
    Request, Response, StatusReport, TopicCount,
};

use crate::{
    metrics::{LATENCY_BUCKETS_MS, SIZE_BUCKETS_BYTES},
    partition::Partition,
    state::BusState,
};

/// Seconds since the Unix epoch.
///
/// A clock before the epoch is not a condition the daemon can act on, so it
/// degrades to 0 rather than failing a request: the only consequence is that
/// age-based retention treats everything as brand new.
#[must_use]
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Milliseconds since the Unix epoch.
///
/// Same clock as [`now_secs`], unpacked to the finer unit the dashboard's
/// post-to-delivery latency needs.
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Outcome of dispatching one request.
pub enum Dispatch {
    /// Send this response and continue.
    Reply(Response),
    /// A `Wait` that found nothing: the server must block, then retry.
    WaitPending { partition: String, pattern: Pattern, label: String, timeout_secs: Option<u64> },
    /// A `Follow`: the server streams until the client disconnects.
    FollowPending { partition: String, pattern: Pattern, label: String },
    /// Shut the daemon down after replying.
    Shutdown,
}

/// Handle one request against the daemon's state.
pub fn dispatch(
    state: &mut BusState,
    request: Request,
    active_waiters: &AtomicU64,
    active_followers: &AtomicU64,
) -> Dispatch {
    state.touch();
    match request {
        Request::Ensure { pattern } => {
            let parsed = match Pattern::parse(&pattern) {
                Ok(p) => p,
                Err(e) => return Dispatch::Reply(error(&e)),
            };
            let name = match PartitionName::parse(parsed.partition()) {
                Ok(n) => n,
                Err(e) => return Dispatch::Reply(error(&e)),
            };
            // Sampled before `partition_mut`, which is what creates it.
            let already_running = state.partition_exists(name.as_str());
            match state.partition_mut(&name) {
                Ok(_) => Dispatch::Reply(Response::Ensured {
                    partition: name.as_str().to_owned(),
                    already_running,
                }),
                Err(e) => Dispatch::Reply(error(&e)),
            }
        }

        Request::Post { topic, body, priority, from, broadcast } => {
            let topic = match Topic::parse(&topic) {
                Ok(t) => t,
                Err(e) => return Dispatch::Reply(error(&e)),
            };
            let name = match PartitionName::parse(topic.partition()) {
                Ok(n) => n,
                Err(e) => return Dispatch::Reply(error(&e)),
            };
            // The size histogram is a bus-level (cross-partition) view, but
            // `publish` only sees its own `PartitionTally`: record the body
            // size here, on the handler, where the partition path has a name
            // and the state owns `BusTally` — the one place both are in scope.
            let body_bytes = u64::try_from(body.len()).unwrap_or(u64::MAX);
            let mut message = Message::new(topic, body, priority, from);
            // Read the stamp before `publish` moves the message: it is the
            // authoritative "sent at" the daemon recorded, and the publisher
            // echoes it back so post and receive agree on one timestamp.
            let ts = message.ts.clone();
            if broadcast {
                message = message.broadcast();
            }
            match state.partition_mut(&name).and_then(|p| p.publish(message)) {
                Ok(id) => {
                    state.bus_tally.record_post(body_bytes);
                    Dispatch::Reply(Response::Posted { id: id.to_string(), ts })
                }
                Err(e) => Dispatch::Reply(error(&e)),
            }
        }

        Request::Wait { pattern, label, timeout_secs } => {
            let (pattern, partition) = match resolve(state, &pattern) {
                Ok(pair) => pair,
                Err(response) => return Dispatch::Reply(response),
            };
            // A matching message may already have been posted before this wait
            // began. Delivery is exclusive: handing it out here advances the
            // pattern's position immediately, so a concurrent waiter cannot
            // also receive it.
            match partition.deliver(&pattern, &label, 1) {
                Ok(batch) if batch.is_empty() => Dispatch::WaitPending {
                    partition: partition.name().to_owned(),
                    pattern,
                    label,
                    timeout_secs,
                },
                Ok(batch) => {
                    record_latency(state, &batch);
                    Dispatch::Reply(Response::Messages { messages: batch })
                }
                Err(e) => Dispatch::Reply(error(&e)),
            }
        }

        Request::Read { pattern, label } => match resolve(state, &pattern) {
            Ok((pattern, partition)) => match partition.deliver(&pattern, &label, usize::MAX) {
                Ok(messages) => {
                    record_latency(state, &messages);
                    Dispatch::Reply(Response::Messages { messages })
                }
                Err(e) => Dispatch::Reply(error(&e)),
            },
            Err(response) => Dispatch::Reply(response),
        },

        Request::Follow { pattern, label } => match resolve(state, &pattern) {
            Ok((pattern, partition)) => {
                Dispatch::FollowPending { partition: partition.name().to_owned(), pattern, label }
            }
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

        Request::Status => Dispatch::Reply(Response::Status { status: build_status(state) }),

        Request::Metrics => {
            let report = build_metrics(state, active_waiters, active_followers);
            Dispatch::Reply(Response::Metrics { metrics: Box::new(report) })
        }

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
    // A pattern's first segment is already a single literal non-empty segment,
    // so this re-validation never rejects a legitimate pattern. It is here so
    // that the only route to a partition file runs through the same check,
    // whatever the caller.
    let name = PartitionName::parse(parsed.partition()).map_err(|e| error(&e))?;
    let partition = state.partition_mut(&name).map_err(|e| error(&e))?;
    Ok((parsed, partition))
}

fn error(e: &dyn std::fmt::Display) -> Response {
    Response::Error { message: e.to_string() }
}

/// Record post-to-delivery latency for a just-delivered batch.
///
/// Kept out of `dispatch` so the request arms stay one line each: the latency
/// histogram is cross-partition, and the recording point is the same for every
/// request that returns messages.
fn record_latency(state: &mut BusState, batch: &[Message]) {
    for m in batch {
        state.record_delivery_latency(m.id.timestamp_ms());
    }
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
            patterns: p
                .pattern_snapshots()
                .into_iter()
                .map(|s| PatternReport {
                    key: s.key,
                    label: s.label,
                    broadcast: s.broadcast,
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

/// Assemble a cumulative metrics snapshot for the dashboard.
///
/// Counters and histograms come from the tallies bumped by publish / deliver /
/// prune; the derived views (`publishers`, `top_topics`, `top_consumers`,
/// per-partition lag) are computed from the in-memory logs, the same way
/// `build_status` does, so they stay consistent with what the log retains.
#[must_use]
pub fn build_metrics(
    state: &BusState,
    active_waiters: &AtomicU64,
    active_followers: &AtomicU64,
) -> MetricsReport {
    let tally = state.bus_tally();
    let t_snap = tally.snapshot();

    let mut totals = MetricsTotals::default();
    let mut partitions = Vec::new();
    let mut all_publishers = std::collections::BTreeSet::new();
    let mut all_topics: std::collections::HashMap<String, (u64, u64)> =
        std::collections::HashMap::new();
    let now = now_secs();

    for p in state.partitions() {
        let p_tally = p.tally();
        totals.posts += p_tally.posts;
        totals.deliveries += p_tally.deliveries;
        totals.bytes_posted += p_tally.bytes;
        totals.posts_high += p_tally.posts_high;
        totals.posts_broadcast += p_tally.posts_broadcast;
        totals.pruned += p_tally.pruned;
        totals.snapped += p_tally.snapped;
        totals.skipped += u64::try_from(p.skipped_records()).unwrap_or(u64::MAX);

        let mut publishers = std::collections::BTreeSet::new();
        let mut lag = 0usize;
        for m in p.log_messages() {
            publishers.insert(m.from.clone());
            all_publishers.insert(m.from.clone());
            let entry = all_topics.entry(m.topic.as_str().to_owned()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += u64::try_from(m.body.len()).unwrap_or(u64::MAX);
        }
        for snap in p.pattern_snapshots() {
            lag += snap.lag;
        }

        partitions.push(PartitionMetrics {
            name: p.name().to_owned(),
            message_count: p.message_count(),
            oldest_age_secs: p.oldest_age_secs(now),
            undelivered_lag: lag,
            participants: publishers.len(),
            posts: p_tally.posts,
            deliveries: p_tally.deliveries,
            bytes: p_tally.bytes,
            pruned: p_tally.pruned,
            skipped: u64::try_from(p.skipped_records()).unwrap_or(u64::MAX),
            snapped: p_tally.snapped,
        });
    }

    let mut top_topics: Vec<TopicCount> = all_topics
        .into_iter()
        .map(|(topic, (count, total_bytes))| TopicCount { topic, count, total_bytes })
        .collect();
    top_topics.sort_by(|a, b| b.count.cmp(&a.count).then(b.total_bytes.cmp(&a.total_bytes)));
    top_topics.truncate(10);

    let mut top_consumers: Vec<ConsumerLag> = Vec::new();
    for p in state.partitions() {
        for snap in p.pattern_snapshots() {
            top_consumers.push(ConsumerLag {
                label: snap.label,
                partition: p.name().to_owned(),
                pattern: snap.key,
                broadcast: snap.broadcast,
                lag: snap.lag,
            });
        }
    }
    top_consumers.sort_by_key(|c| std::cmp::Reverse(c.lag));
    top_consumers.truncate(10);

    MetricsReport {
        pid: std::process::id(),
        uptime_secs: state.uptime_secs(),
        poll_at_ms: now_millis(),
        totals,
        latency_buckets_ms: LATENCY_BUCKETS_MS.to_vec(),
        latency_histogram_ms: t_snap.latency_hist.to_vec(),
        size_buckets_bytes: SIZE_BUCKETS_BYTES.to_vec(),
        size_histogram_bytes: t_snap.size_hist.to_vec(),
        active_waiters: active_waiters.load(Ordering::Relaxed),
        active_followers: active_followers.load(Ordering::Relaxed),
        partitions,
        publishers: all_publishers.into_iter().collect(),
        top_topics,
        top_consumers,
    }
}
#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use tempfile::TempDir;

    use agent_bus_core::Priority;

    use super::*;

    #[test]
    fn build_metrics_reports_cumulative_counts_after_activity() {
        let dir = TempDir::new().unwrap();
        let mut state = BusState::new(dir.path().to_path_buf());
        let name = PartitionName::parse("iot_base").unwrap();
        let topic = Topic::parse("iot_base/dev_01").unwrap();

        for body in ["one", "two", "three"] {
            let m = Message::new(topic.clone(), body.to_owned(), Priority::Normal, None);
            state.partition_mut(&name).unwrap().publish(m).unwrap();
        }
        // Deliver one via dispatch, the same path the latency histogram is
        // recorded on, so the assert below exercises the real recording point.
        let outcome = dispatch(
            &mut state,
            Request::Wait {
                pattern: "iot_base/**".to_owned(),
                label: "rev".to_owned(),
                timeout_secs: Some(1),
            },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
        );
        assert!(matches!(outcome, Dispatch::Reply(Response::Messages { .. })));

        let aw = AtomicU64::new(0);
        let af = AtomicU64::new(0);
        let report = build_metrics(&state, &aw, &af);
        assert_eq!(report.totals.posts, 3);
        assert_eq!(report.totals.deliveries, 1);
        assert_eq!(report.totals.bytes_posted, 11); // "one"+"two"+"three"
        assert_eq!(report.partitions.len(), 1);
        assert_eq!(report.partitions[0].name, "iot_base");
        assert_eq!(report.partitions[0].participants, 1);
        assert!(report.latency_histogram_ms.iter().sum::<u64>() >= 1);
    }

    #[test]
    fn build_metrics_active_counters_reflect_the_instantaneous_state() {
        let dir = TempDir::new().unwrap();
        let state = BusState::new(dir.path().to_path_buf());
        let aw = AtomicU64::new(2);
        let af = AtomicU64::new(0);
        let report = build_metrics(&state, &aw, &af);
        assert_eq!(report.active_waiters, 2);
        assert_eq!(report.active_followers, 0);
    }
}
