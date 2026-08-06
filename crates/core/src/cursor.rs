use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::error::CoreError;

/// Read positions within one partition, keyed by subscriber *and* pattern.
///
/// A position is the id of the last message that subscriber consumed
/// successfully for that pattern, so "unread" means "id strictly greater than
/// this".
///
/// The pattern is part of the key because delivery is filtered by pattern:
/// `unread` seeks by cursor and then keeps only matching messages. With one
/// cursor per subscriber, acknowledging the last *delivered* id also marked
/// every intervening *non-matching* message as consumed, so a `wait` on
/// `iot_base/dev_01` silently destroyed an unread `iot_base/planner` message
/// for the same subscriber. Keying on the pair makes the stored position mean
/// what the query already meant: "how far this subscriber has read *this
/// pattern*".
///
/// Represented as a nested map — subscriber → pattern → id — rather than a
/// flattened `"{sub}:{pattern}"` string key. Both components are arbitrary
/// client-supplied strings that may contain any separator, so a flat key would
/// need an escaping scheme to stay unambiguous; nesting removes the question.
/// It also serializes to a plain JSON object, which a `BTreeMap` with a tuple
/// key does not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CursorStore {
    positions: BTreeMap<String, BTreeMap<String, Ulid>>,
}

impl CursorStore {
    #[must_use]
    pub fn position(&self, subscriber: &str, pattern: &str) -> Option<Ulid> {
        self.positions.get(subscriber)?.get(pattern).copied()
    }

    /// Move one (subscriber, pattern) cursor forward. Never moves it backwards,
    /// so a duplicate or out-of-order acknowledgement cannot cause redelivery.
    pub fn advance(&mut self, subscriber: &str, pattern: &str, id: Ulid) {
        self.positions
            .entry(subscriber.to_owned())
            .or_default()
            .entry(pattern.to_owned())
            .and_modify(|current| {
                if id > *current {
                    *current = id;
                }
            })
            .or_insert(id);
    }

    /// After pruning, drag any cursor older than `oldest_surviving` forward to
    /// it, and report which (subscriber, pattern) pairs were affected.
    ///
    /// Without this a subscriber whose cursor pointed into pruned history would
    /// be handed the entire surviving log as "unread".
    ///
    /// Returns pairs rather than bare subscriber ids because one subscriber can
    /// hold several pattern cursors and they snap independently; collapsing to
    /// the id alone would report a subscriber as having missed messages on
    /// patterns whose cursors were untouched.
    pub fn snap_forward(&mut self, oldest_surviving: Ulid) -> Vec<(String, String)> {
        let mut snapped = Vec::new();
        for (subscriber, patterns) in &mut self.positions {
            for (pattern, position) in patterns.iter_mut() {
                if *position < oldest_surviving {
                    *position = oldest_surviving;
                    snapped.push((subscriber.clone(), pattern.clone()));
                }
            }
        }
        snapped
    }

    /// Every (subscriber, pattern, position) triple, for `status`.
    // No `#[must_use]`: `impl Iterator` already carries it, and repeating it
    // trips clippy's `double_must_use`.
    pub fn subscribers(&self) -> impl Iterator<Item = (&str, &str, Ulid)> {
        self.positions.iter().flat_map(|(subscriber, patterns)| {
            patterns.iter().map(move |(pattern, id)| (subscriber.as_str(), pattern.as_str(), *id))
        })
    }

    /// # Errors
    /// Returns [`CoreError::MalformedRecord`] if serialization fails.
    pub fn to_json(&self) -> Result<String, CoreError> {
        serde_json::to_string(self).map_err(|e| CoreError::MalformedRecord(e.to_string()))
    }

    /// Parse a cursor file. Empty input yields an empty store, so a missing or
    /// freshly created file is not an error.
    ///
    /// # Errors
    /// Returns [`CoreError::MalformedRecord`] if non-empty input is not valid
    /// JSON in the current nested `subscriber -> pattern -> id` shape. A file
    /// written by an older build — flat `subscriber -> id` — lands here too;
    /// see [`Self::from_json_or_reset`] for the recovery the daemon applies.
    pub fn from_json(input: &str) -> Result<Self, CoreError> {
        if input.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(input).map_err(|e| CoreError::MalformedRecord(e.to_string()))
    }

    /// Parse a cursor file, falling back to an empty store on unreadable input.
    ///
    /// Returns the store and, when the input could not be parsed, the reason —
    /// which the caller is expected to log. The reason is handed back rather
    /// than printed here because this crate does no I/O.
    ///
    /// Resetting rather than failing is deliberate. The likely cause is a
    /// cursor file written by a build that keyed cursors on the subscriber
    /// alone; that format is not mechanically convertible, since a flat
    /// position carries no record of which pattern earned it. There is no
    /// migration by design: this is pre-1.0 software with a one-hour retention
    /// window, so the worst outcome of starting fresh is that a subscriber is
    /// redelivered up to an hour of messages it had already seen. `wait` and
    /// `read` are at-least-once, so redelivery is a case callers already
    /// handle — whereas refusing to open the partition would take the whole
    /// bus down over a file that will be irrelevant within the hour.
    ///
    /// Losing the positions silently would not be defensible; that is why the
    /// reason comes back instead of being swallowed.
    #[must_use]
    pub fn from_json_or_reset(input: &str) -> (Self, Option<String>) {
        match Self::from_json(input) {
            Ok(store) => (store, None),
            Err(e) => (Self::default(), Some(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` ids that are strictly increasing *by construction*.
    ///
    /// Deliberately not `Ulid::new()`: that is clock-derived and not monotonic
    /// within a millisecond, so a tight loop can return equal or descending
    /// ids and silently defang every ordering assertion below. Fixing the
    /// timestamp component makes the ordering visible in the test rather than
    /// dependent on how fast the machine runs.
    fn ids(n: usize) -> Vec<Ulid> {
        (0..n).map(|i| Ulid::from_parts(1_700_000_000_000 + u64::try_from(i).unwrap(), 0)).collect()
    }

    #[test]
    fn ids_helper_is_strictly_increasing() {
        let v = ids(50);
        assert!(v.windows(2).all(|w| w[0] < w[1]), "test helper must produce ordered ids");
    }

    const P: &str = "iot_base/**";

    #[test]
    fn unknown_subscriber_starts_at_no_position() {
        let store = CursorStore::default();
        assert_eq!(store.position("reviewer", P), None);
    }

    #[test]
    fn advance_records_position() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("reviewer", P, v[0]);
        assert_eq!(store.position("reviewer", P), Some(v[0]));
        store.advance("reviewer", P, v[1]);
        assert_eq!(store.position("reviewer", P), Some(v[1]));
    }

    #[test]
    fn advance_never_moves_backwards() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("reviewer", P, v[1]);
        store.advance("reviewer", P, v[0]);
        assert_eq!(store.position("reviewer", P), Some(v[1]), "cursor must not rewind");
    }

    #[test]
    fn subscribers_are_independent() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("reviewer", P, v[0]);
        store.advance("planner", P, v[1]);
        assert_eq!(store.position("reviewer", P), Some(v[0]));
        assert_eq!(store.position("planner", P), Some(v[1]));
    }

    /// The regression that motivated the composite key: one subscriber reading
    /// two patterns must keep two independent positions, so acknowledging a
    /// message delivered for one pattern cannot mark the other's as consumed.
    #[test]
    fn one_subscriber_keeps_a_separate_cursor_per_pattern() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("w1", "iot_base/dev_01", v[1]);
        assert_eq!(store.position("w1", "iot_base/dev_01"), Some(v[1]));
        assert_eq!(
            store.position("w1", "iot_base/planner"),
            None,
            "acking one pattern must not move another pattern's cursor"
        );
    }

    #[test]
    fn snap_forward_moves_stale_cursors_only() {
        let mut store = CursorStore::default();
        let v = ids(3);
        store.advance("stale", P, v[0]);
        store.advance("fresh", P, v[2]);
        // v[1] is now the oldest surviving message.
        let snapped = store.snap_forward(v[1]);
        assert_eq!(snapped, vec![("stale".to_owned(), P.to_owned())]);
        assert_eq!(store.position("stale", P), Some(v[1]));
        assert_eq!(store.position("fresh", P), Some(v[2]), "fresh cursor untouched");
    }

    /// Snapping is per pattern: a subscriber with a stale cursor on one pattern
    /// and a current one on another must be reported for the stale pattern
    /// only, not flagged wholesale.
    #[test]
    fn snap_forward_reports_the_affected_pattern_not_the_whole_subscriber() {
        let mut store = CursorStore::default();
        let v = ids(3);
        store.advance("w1", "iot_base/dev_01", v[0]);
        store.advance("w1", "iot_base/planner", v[2]);
        let snapped = store.snap_forward(v[1]);
        assert_eq!(snapped, vec![("w1".to_owned(), "iot_base/dev_01".to_owned())]);
        assert_eq!(store.position("w1", "iot_base/planner"), Some(v[2]));
    }

    /// A cursor sitting exactly on the oldest surviving message has already
    /// consumed it and missed nothing, so it must be left alone and must not
    /// be reported in `status` as having lost messages.
    #[test]
    fn snap_forward_leaves_a_cursor_exactly_at_the_boundary() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("boundary", P, v[1]);
        let snapped = store.snap_forward(v[1]);
        assert!(snapped.is_empty(), "a cursor at the boundary has not missed anything");
        assert_eq!(store.position("boundary", P), Some(v[1]));
    }

    #[test]
    fn roundtrips_through_json() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("reviewer", "iot_base/**", v[0]);
        store.advance("reviewer", "iot_base/planner", v[1]);
        let json = store.to_json().unwrap();
        let back = CursorStore::from_json(&json).unwrap();
        assert_eq!(back.position("reviewer", "iot_base/**"), Some(v[0]));
        assert_eq!(back.position("reviewer", "iot_base/planner"), Some(v[1]));
    }

    /// The composite key must survive the round trip as a real JSON object, not
    /// as a stringified tuple — a `BTreeMap` with a tuple key silently
    /// serializes to an array and would not reload as an object.
    #[test]
    fn json_shape_is_a_nested_object() {
        let mut store = CursorStore::default();
        let v = ids(1);
        store.advance("reviewer", "iot_base/**", v[0]);
        let json = store.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["reviewer"]["iot_base/**"], serde_json::json!(v[0].to_string()));
    }

    /// Subscriber ids and patterns are arbitrary strings that may contain any
    /// separator a flat key encoding would have picked. Nesting has no
    /// separator to collide with, and this pins that.
    #[test]
    fn keys_containing_separators_stay_distinct() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("a:b", "c", v[0]);
        store.advance("a", "b:c", v[1]);
        let back = CursorStore::from_json(&store.to_json().unwrap()).unwrap();
        assert_eq!(back.position("a:b", "c"), Some(v[0]));
        assert_eq!(back.position("a", "b:c"), Some(v[1]));
    }

    #[test]
    fn from_json_tolerates_empty_input() {
        assert_eq!(CursorStore::from_json("").unwrap().position("x", P), None);
    }

    /// A cursor file from the old flat `subscriber -> id` format is not
    /// convertible, so it must reset to empty *and* hand back a reason for the
    /// caller to log rather than dropping the positions silently.
    #[test]
    fn old_format_resets_and_reports_a_reason() {
        let old = r#"{"reviewer":"01J000000000000000000000"}"#;
        assert!(CursorStore::from_json(old).is_err(), "the old flat format must not parse");

        let (store, reason) = CursorStore::from_json_or_reset(old);
        assert_eq!(store.position("reviewer", P), None);
        assert!(reason.is_some(), "an unreadable cursor file must not be discarded silently");
    }

    #[test]
    fn from_json_or_reset_is_quiet_on_valid_input() {
        let mut store = CursorStore::default();
        let v = ids(1);
        store.advance("reviewer", P, v[0]);
        let (back, reason) = CursorStore::from_json_or_reset(&store.to_json().unwrap());
        assert_eq!(reason, None, "a readable file must not be reported as damaged");
        assert_eq!(back.position("reviewer", P), Some(v[0]));
    }
}
