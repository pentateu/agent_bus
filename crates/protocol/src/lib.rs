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
    },
    Messages {
        messages: Vec<Message>,
    },
    /// `Wait` hit its timeout with nothing to deliver. Maps to exit code 2.
    Timeout,
    Status {
        status: StatusReport,
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
}
