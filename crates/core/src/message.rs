use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::{error::CoreError, topic::Topic};

/// Delivery hint carried with a message.
///
/// The bus does not act on this. Hook adapters render different instruction
/// text for `High`, which is how "interrupt me" versus "queue it" is expressed
/// without the daemon knowing anything about agent policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    #[default]
    Normal,
    High,
}

/// One published message.
///
/// `id` is a ULID: sortable by creation time, so it doubles as the cursor
/// value and lets "everything after my cursor" be a binary search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: Ulid,
    /// RFC 3339 timestamp, for humans reading the log.
    pub ts: String,
    pub topic: Topic,
    #[serde(default)]
    pub priority: Priority,
    pub from: String,
    pub body: String,
}

impl Message {
    /// Build a message, assigning a fresh monotonic id and timestamp.
    ///
    /// When `from` is `None` it defaults to the last segment of the topic,
    /// which is almost always the posting agent's name.
    #[must_use]
    pub fn new(topic: Topic, body: String, priority: Priority, from: Option<String>) -> Self {
        let from = from.unwrap_or_else(|| {
            topic.as_str().rsplit('/').next().unwrap_or(topic.as_str()).to_owned()
        });
        Self { id: next_id(), ts: now_rfc3339(), topic, priority, from, body }
    }

    /// Serialize to a single JSONL line (no trailing newline).
    ///
    /// # Errors
    /// Returns [`CoreError::MalformedRecord`] if serialization fails.
    pub fn to_jsonl(&self) -> Result<String, CoreError> {
        serde_json::to_string(self).map_err(|e| CoreError::MalformedRecord(e.to_string()))
    }

    /// Parse one JSONL line.
    ///
    /// # Errors
    /// Returns [`CoreError::MalformedRecord`] if the line is not a valid record.
    pub fn from_jsonl(line: &str) -> Result<Self, CoreError> {
        serde_json::from_str(line).map_err(|e| CoreError::MalformedRecord(e.to_string()))
    }

    /// Age in seconds relative to `now_unix_secs`, saturating at zero.
    #[must_use]
    pub fn age_secs(&self, now_unix_secs: u64) -> u64 {
        // `timestamp_ms` is a u64 count of milliseconds, so the division is
        // exact and stays in range; no cast is involved.
        now_unix_secs.saturating_sub(self.id.timestamp_ms() / 1000)
    }
}

/// Process-global monotonic ULID source.
///
/// `Ulid::new()` derives purely from the clock, so two calls inside the same
/// millisecond can produce ids that do not compare greater. Cursors are ULIDs,
/// so a tie would let a reader silently skip or re-read a message. The shared
/// [`ulid::Generator`] bumps the random component instead of colliding.
static GENERATOR: Mutex<ulid::Generator> = Mutex::new(ulid::Generator::new());

fn next_id() -> Ulid {
    let mut guard = GENERATOR.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Overflow needs 2^80 ids within one millisecond; committing the increment
    // still yields a monotonic id, which is the property we actually depend on.
    guard.generate().unwrap_or_else(ulid::Overflow::commit_overflow_increment)
}

/// Current time as an RFC 3339 / ISO 8601 UTC string with millisecond precision.
///
/// Hand-rolled to avoid a `chrono`/`time` dependency for one format string.
fn now_rfc3339() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();

    let days = secs / 86_400;
    let seconds_of_day = secs % 86_400;
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    // A `u64` second count only exceeds `i64` days past year 292 billion; the
    // fallback pins us to the epoch rather than panicking on a nonsense clock.
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert days since the Unix epoch to a civil (year, month, day).
///
/// Howard Hinnant's `civil_from_days` algorithm, kept in `i64` throughout so
/// that no lossy cast is needed: every intermediate is bounded by the era
/// length (`146_097` days) or by the input year, both far inside `i64`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    // `doe` ("day of era") is in `0..146_097` by construction, so it and every
    // value derived from it below are small non-negative integers.
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    // `d` is in `1..=31` and `m` in `1..=12`, so both conversions succeed; the
    // zero fallback keeps this function total instead of panicking.
    (if m <= 2 { y + 1 } else { y }, u32::try_from(m).unwrap_or(0), u32::try_from(d).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic() -> Topic {
        Topic::parse("iot_base/dev_01").unwrap()
    }

    #[test]
    fn new_message_has_sortable_id_and_defaults() {
        let m = Message::new(topic(), "ready for review".to_owned(), Priority::Normal, None);
        assert_eq!(m.topic.as_str(), "iot_base/dev_01");
        assert_eq!(m.body, "ready for review");
        assert_eq!(m.priority, Priority::Normal);
        // `from` defaults to the last segment of the topic.
        assert_eq!(m.from, "dev_01");
    }

    #[test]
    fn explicit_from_overrides_default() {
        let m = Message::new(topic(), "x".to_owned(), Priority::High, Some("planner".to_owned()));
        assert_eq!(m.from, "planner");
        assert_eq!(m.priority, Priority::High);
    }

    #[test]
    fn ids_increase_monotonically() {
        let a = Message::new(topic(), "a".to_owned(), Priority::Normal, None);
        let b = Message::new(topic(), "b".to_owned(), Priority::Normal, None);
        assert!(b.id > a.id, "ULIDs must be monotonic within a process");
    }

    /// A tighter version of the above: a tight loop is near-certain to put
    /// several ids inside the same millisecond, which is exactly the case a
    /// clock-derived `Ulid::new()` gets wrong.
    #[test]
    fn ids_are_monotonic_under_rapid_creation() {
        let mut previous = Message::new(topic(), "0".to_owned(), Priority::Normal, None).id;
        for i in 1..1000 {
            let next = Message::new(topic(), i.to_string(), Priority::Normal, None).id;
            assert!(next > previous, "id {i} was not greater than its predecessor");
            previous = next;
        }
    }

    #[test]
    fn roundtrips_through_jsonl() {
        let m = Message::new(topic(), "hello\nworld".to_owned(), Priority::High, None);
        let line = m.to_jsonl().unwrap();
        assert!(!line.contains('\n'), "a JSONL record must occupy exactly one line");
        let back = Message::from_jsonl(&line).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn rejects_malformed_jsonl() {
        assert!(Message::from_jsonl("not json").is_err());
        assert!(Message::from_jsonl(r#"{"id":"x"}"#).is_err());
    }

    #[test]
    fn priority_defaults_to_normal_when_absent() {
        assert_eq!(Priority::default(), Priority::Normal);
    }

    #[test]
    fn timestamp_has_expected_shape() {
        let ts = now_rfc3339();
        assert_eq!(ts.len(), 24, "expected `YYYY-MM-DDTHH:MM:SS.mmmZ`, got {ts:?}");
        assert!(ts.ends_with('Z'), "timestamp must be UTC-qualified: {ts:?}");

        // Derive the expected year from the same clock the formatter reads, so
        // the test does not go stale and does not flake on New Year's Eve.
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let (year, _, _) = civil_from_days(i64::try_from(secs / 86_400).unwrap());
        assert!(ts.starts_with(&format!("{year:04}-")), "unexpected year in {ts:?}");
    }
}

