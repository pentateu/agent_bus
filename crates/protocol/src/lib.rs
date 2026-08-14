//! The wire format shared by the daemon and the CLI.
//!
//! One JSON object per line in each direction. Kept in its own crate so the
//! two sides cannot drift apart.
#![cfg_attr(test, allow(clippy::unwrap_used))]

use agent_bus_core::Message;
pub use agent_bus_core::Priority;
use serde::{Deserialize, Serialize};

/// A client request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Ensure the daemon and a partition exist. Used by `agent-bus daemon`.
    Ensure {
        pattern: String,
    },
    Post {
        topic: String,
        body: String,
        priority: Priority,
        from: Option<String>,
        /// Broadcast this message to every consumer (distinct `--as` label)
        /// whose pattern matches the topic, each getting their own copy once.
        /// Normal messages are exclusive per pattern.
        #[serde(default)]
        broadcast: bool,
    },
    /// Block until a message newer than the pattern's position is available.
    ///
    /// `label` is the `--as` name, shown in `status` and used for nothing else:
    /// delivery is exclusive per pattern, so it does not create a position.
    Wait {
        pattern: String,
        label: String,
        timeout_secs: Option<u64>,
    },
    /// Return everything unread right now, without blocking.
    ///
    /// `label` is the `--as` name, shown in `status` and used for nothing else.
    Read {
        pattern: String,
        label: String,
    },
    /// Stream messages indefinitely.
    ///
    /// `label` is the `--as` name, shown in `status` and used for nothing else.
    Follow {
        pattern: String,
        label: String,
    },
    /// Replay a time window, ignoring the cursor. `since_secs: None` means the
    /// full retained window.
    History {
        pattern: String,
        since_secs: Option<u64>,
    },
    Status,
    /// Cumulative counters + histograms since the daemon started, for the
    /// dashboard. One snapshot per poll; the caller diffs successive snapshots
    /// into a time series.
    Metrics,
    Stop,
}

/// A daemon response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Ensured {
        partition: String,
        already_running: bool,
    },
    Posted {
        id: String,
        /// RFC 3339 timestamp of when the message was stamped on send, so a
        /// publisher can report when its post went out without racing a clock.
        ts: String,
    },
    Messages {
        messages: Vec<Message>,
    },
    /// `Wait` hit its timeout with nothing to deliver. Maps to exit code 2.
    Timeout,
    Status {
        status: StatusReport,
    },
    Metrics {
        /// Boxed so the response enum stays small: a report is read out of a
        /// connection buffer and never matched on repeatedly.
        metrics: Box<MetricsReport>,
    },
    Error {
        message: String,
    },
}

/// Snapshot of daemon state, for `agent-bus status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub pid: u32,
    pub uptime_secs: u64,
    pub socket_path: String,
    pub partitions: Vec<PartitionReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionReport {
    pub name: String,
    pub message_count: usize,
    pub oldest_age_secs: Option<u64>,
    /// One line per pattern delivery position.
    pub patterns: Vec<PatternReport>,
    /// Corrupt log lines skipped at load time. Surfaced rather than swallowed.
    pub skipped_records: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternReport {
    /// The position key: a pattern string for exclusive delivery, a consumer
    /// label for broadcast delivery.
    pub key: String,
    /// The most recent `--as` label used with this pattern, if any. A label is
    /// a name, never an exclusive delivery identity: it does not create a
    /// second exclusive position.
    pub label: String,
    /// True when this is a broadcast (per-label) position rather than an
    /// exclusive (per-pattern) position.
    pub broadcast: bool,
    pub cursor: String,
    /// Unread messages behind this cursor.
    pub lag: usize,
    /// True if this cursor was dragged forward by pruning, meaning messages
    /// provably missed.
    pub snapped: bool,
}

/// Cumulative daemon metrics since daemon start, for `agent-bus dashboard`.
///
/// All counters and histograms are monotonically non-decreasing for the life
/// of one daemon process. A client that polls repeatedly derives rates by
/// differencing successive snapshots; a daemon restart resets everything and
/// the client must rebaseline (see the design spec).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsReport {
    pub pid: u32,
    pub uptime_secs: u64,
    /// Wall-clock milliseconds at the daemon when this snapshot was built.
    /// Lets the dashboard render "x seconds ago" without trusting its own
    /// clock for cross-process comparisons.
    pub poll_at_ms: u64,
    pub totals: MetricsTotals,
    /// Lower bounds of each latency bin. Always begins with 0; the histogram
    /// has one more cell than this (the last is `>=` the final boundary).
    pub latency_buckets_ms: Vec<u64>,
    pub latency_histogram_ms: Vec<u64>,
    pub size_buckets_bytes: Vec<u64>,
    pub size_histogram_bytes: Vec<u64>,
    /// Connections currently parked in `wait`. Instantaneous, not cumulative.
    pub active_waiters: u64,
    /// Connections currently streaming via `follow`.
    pub active_followers: u64,
    pub partitions: Vec<PartitionMetrics>,
    /// Distinct `from` values across all retained logs, sorted.
    pub publishers: Vec<String>,
    /// Concrete topics in the retained window, top 10 by count.
    pub top_topics: Vec<TopicCount>,
    /// Per-position lag, top 10 by lag descending.
    pub top_consumers: Vec<ConsumerLag>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MetricsTotals {
    pub posts: u64,
    pub deliveries: u64,
    pub bytes_posted: u64,
    pub posts_high: u64,
    pub posts_broadcast: u64,
    pub pruned: u64,
    /// Corrupt log lines skipped at load. Comes from `PartitionLog`; not a
    /// runtime increment, surfaced for visibility.
    pub skipped: u64,
    pub snapped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionMetrics {
    pub name: String,
    pub message_count: usize,
    pub oldest_age_secs: Option<u64>,
    /// Unread messages behind every position in this partition.
    pub undelivered_lag: usize,
    /// Distinct `from` values in this partition's retained log.
    pub participants: usize,
    pub posts: u64,
    pub deliveries: u64,
    pub bytes: u64,
    pub pruned: u64,
    pub skipped: u64,
    pub snapped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicCount {
    pub topic: String,
    pub count: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumerLag {
    pub label: String,
    pub partition: String,
    pub pattern: String,
    pub broadcast: bool,
    pub lag: usize,
}

/// Encode a frame as one line (no trailing newline).
///
/// # Errors
/// Returns the underlying `serde_json` error if the value cannot be serialized.
pub fn encode<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

/// Decode one line.
///
/// # Errors
/// Returns the underlying `serde_json` error if the line is not a valid frame.
pub fn decode<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let req = Request::Post {
            topic: "iot_base/dev_01".to_owned(),
            body: "ready".to_owned(),
            priority: Priority::High,
            from: Some("dev_01".to_owned()),
            broadcast: true,
        };
        let line = encode(&req).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(decode::<Request>(&line).unwrap(), req);
    }

    #[test]
    fn response_roundtrips() {
        let res: Response = Response::Error { message: "nope".to_owned() };
        let line = encode(&res).unwrap();
        assert_eq!(decode::<Response>(&line).unwrap(), res);
    }

    #[test]
    fn unknown_variant_is_an_error_not_a_panic() {
        assert!(decode::<Request>(r#"{"type":"nonexistent"}"#).is_err());
    }

    #[test]
    fn unit_variant_requests_roundtrip() {
        for req in [Request::Status, Request::Stop] {
            let line = encode(&req).unwrap();
            assert_eq!(decode::<Request>(&line).unwrap(), req);
        }
    }

    #[test]
    fn unit_variant_responses_roundtrip() {
        for res in [Response::Ok, Response::Timeout] {
            let line = encode(&res).unwrap();
            assert_eq!(decode::<Response>(&line).unwrap(), res);
        }
    }

    #[test]
    fn status_variant_with_nested_report_roundtrips() {
        let res = Response::Status {
            status: StatusReport {
                pid: 1234,
                uptime_secs: 60,
                socket_path: "/tmp/agent-bus.sock".to_owned(),
                partitions: vec![PartitionReport {
                    name: "iot_base/dev_01".to_owned(),
                    message_count: 3,
                    oldest_age_secs: Some(120),
                    patterns: vec![PatternReport {
                        key: "iot_base/**".to_owned(),
                        label: "reviewer".to_owned(),
                        broadcast: false,
                        cursor: "01J000000000000000000000".to_owned(),
                        lag: 1,
                        snapped: false,
                    }],
                    skipped_records: 0,
                }],
            },
        };
        let line = encode(&res).unwrap();
        assert_eq!(decode::<Response>(&line).unwrap(), res);
    }

    #[test]
    fn ensure_request_roundtrips() {
        let ensure = Request::Ensure { pattern: "iot_base/*".to_owned() };
        let line = encode(&ensure).unwrap();
        assert_eq!(decode::<Request>(&line).unwrap(), ensure);
    }

    #[test]
    fn metrics_request_roundtrips() {
        let req = Request::Metrics;
        let line = encode(&req).unwrap();
        assert_eq!(decode::<Request>(&line).unwrap(), req);
    }

    #[test]
    fn metrics_report_roundtrips() {
        let report = MetricsReport {
            pid: 42,
            uptime_secs: 300,
            poll_at_ms: 1_700_000_000_000,
            totals: MetricsTotals {
                posts: 5,
                deliveries: 4,
                bytes_posted: 900,
                posts_high: 1,
                posts_broadcast: 2,
                pruned: 0,
                skipped: 0,
                snapped: 0,
            },
            latency_buckets_ms: vec![0, 10, 25],
            latency_histogram_ms: vec![3, 1, 0, 0],
            size_buckets_bytes: vec![0, 64, 256],
            size_histogram_bytes: vec![1, 3, 1, 0],
            active_waiters: 1,
            active_followers: 0,
            partitions: vec![PartitionMetrics {
                name: "iot_base".to_owned(),
                message_count: 2,
                oldest_age_secs: Some(30),
                undelivered_lag: 1,
                participants: 2,
                posts: 5,
                deliveries: 4,
                bytes: 900,
                pruned: 0,
                skipped: 0,
                snapped: 0,
            }],
            publishers: vec!["dev_01".to_owned()],
            top_topics: vec![TopicCount {
                topic: "iot_base/dev_01".to_owned(),
                count: 4,
                total_bytes: 800,
            }],
            top_consumers: vec![ConsumerLag {
                label: "reviewer_01".to_owned(),
                partition: "iot_base".to_owned(),
                pattern: "iot_base/**".to_owned(),
                broadcast: false,
                lag: 1,
            }],
        };
        let res = Response::Metrics { metrics: Box::new(report.clone()) };
        let line = encode(&res).unwrap();
        assert_eq!(decode::<Response>(&line).unwrap(), res);
    }

    #[test]
    fn histogram_has_one_more_cell_than_boundaries() {
        // A documented invariant the daemon is expected to uphold; a client
        // that wanted to validate could check it on decode, so the type itself
        // does not enforce a length and the test just documents the shape.
        let report = sample_report_for_shape_test();
        assert_eq!(report.latency_histogram_ms.len(), report.latency_buckets_ms.len() + 1);
        assert_eq!(report.size_histogram_bytes.len(), report.size_buckets_bytes.len() + 1);
    }

    fn sample_report_for_shape_test() -> MetricsReport {
        MetricsReport {
            pid: 0,
            uptime_secs: 0,
            poll_at_ms: 0,
            totals: MetricsTotals::default(),
            latency_buckets_ms: vec![0, 10],
            latency_histogram_ms: vec![0, 0, 0],
            size_buckets_bytes: vec![0, 64],
            size_histogram_bytes: vec![0, 0, 0],
            active_waiters: 0,
            active_followers: 0,
            partitions: vec![],
            publishers: vec![],
            top_topics: vec![],
            top_consumers: vec![],
        }
    }
}
