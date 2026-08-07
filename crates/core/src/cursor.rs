use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::error::CoreError;

/// Read positions within one partition.
///
/// There are two independent sets of positions, because there are two delivery
/// modes:
///
/// * **Exclusive** positions are keyed by pattern. Delivery is first-consumer-
///   wins: the daemon advances a pattern's position atomically when it hands a
///   message out, so the message is delivered to exactly one consumer of that
///   pattern and never again — to the same consumer, to another consumer, or to
///   any overlapping pattern. `--as` labels play no part here.
///
/// * **Broadcast** positions are keyed by consumer label. A broadcast message
///   is delivered to every distinct label whose pattern matches its topic, each
///   getting their own copy exactly once. Sharing a label means sharing a
///   broadcast position, so two consumers that use the same `--as` value would
///   split broadcast delivery; give each consumer its own label.
///
/// The only way to see a delivered message again is `history`, which ignores
/// positions entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CursorStore {
    /// Exclusive per-pattern positions: pattern -> last delivered id.
    exclusive: BTreeMap<String, Ulid>,
    /// Broadcast per-label positions: label -> last delivered id.
    broadcast: BTreeMap<String, Ulid>,
}

impl CursorStore {
    #[must_use]
    pub fn exclusive_position(&self, pattern: &str) -> Option<Ulid> {
        self.exclusive.get(pattern).copied()
    }

    /// Move one pattern's exclusive position forward. Never moves it backwards,
    /// so a duplicate or out-of-order delivery cannot cause redelivery.
    pub fn advance_exclusive(&mut self, pattern: &str, id: Ulid) {
        self.exclusive
            .entry(pattern.to_owned())
            .and_modify(|current| {
                if id > *current {
                    *current = id;
                }
            })
            .or_insert(id);
    }

    #[must_use]
    pub fn broadcast_position(&self, label: &str) -> Option<Ulid> {
        self.broadcast.get(label).copied()
    }

    /// Move one label's broadcast position forward. Never moves it backwards.
    pub fn advance_broadcast(&mut self, label: &str, id: Ulid) {
        self.broadcast
            .entry(label.to_owned())
            .and_modify(|current| {
                if id > *current {
                    *current = id;
                }
            })
            .or_insert(id);
    }

    /// After pruning, drag any cursor older than `oldest_surviving` forward to
    /// it, and report which keys were affected.
    ///
    /// Without this a cursor that pointed into pruned history would be handed
    /// the entire surviving log as "unread".
    ///
    /// Returns pattern strings for exclusive positions and label strings for
    /// broadcast positions, prefixed to tell them apart so `status` can report
    /// which kind of position was snapped.
    pub fn snap_forward(&mut self, oldest_surviving: Ulid) -> Vec<String> {
        let mut snapped = Vec::new();
        for (pattern, position) in &mut self.exclusive {
            if *position < oldest_surviving {
                *position = oldest_surviving;
                snapped.push(format!("exclusive:{pattern}"));
            }
        }
        for (label, position) in &mut self.broadcast {
            if *position < oldest_surviving {
                *position = oldest_surviving;
                snapped.push(format!("broadcast:{label}"));
            }
        }
        snapped
    }

    /// Every (pattern, position) pair for exclusive delivery, for `status`.
    // No `#[must_use]`: `impl Iterator` already carries it, and repeating it
    // trips clippy's `double_must_use`.
    pub fn exclusive_cursors(&self) -> impl Iterator<Item = (&str, Ulid)> {
        self.exclusive.iter().map(|(pattern, id)| (pattern.as_str(), *id))
    }

    /// Every (label, position) pair for broadcast delivery, for `status`.
    pub fn broadcast_cursors(&self) -> impl Iterator<Item = (&str, Ulid)> {
        self.broadcast.iter().map(|(label, id)| (label.as_str(), *id))
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
    /// JSON in the current `{ exclusive, broadcast }` shape. A file written by
    /// an older build lands here too; see [`Self::from_json_or_reset`] for the
    /// recovery the daemon applies.
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
    /// Resetting rather than failing is deliberate. Older cursor formats are
    /// not mechanically convertible, and there is no migration by design: this
    /// is pre-1.0 software with a one-hour retention window, so the worst
    /// outcome of starting fresh is that unread messages are redelivered —
    /// which, under exclusive delivery, means they are handed out again exactly
    /// once. Whereas refusing to open the partition would take the whole bus
    /// down over a file that will be irrelevant within the hour.
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
    fn unknown_position_starts_at_none() {
        let store = CursorStore::default();
        assert_eq!(store.exclusive_position(P), None);
        assert_eq!(store.broadcast_position("reviewer"), None);
    }

    #[test]
    fn advance_records_exclusive_position() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance_exclusive(P, v[0]);
        assert_eq!(store.exclusive_position(P), Some(v[0]));
        store.advance_exclusive(P, v[1]);
        assert_eq!(store.exclusive_position(P), Some(v[1]));
    }

    #[test]
    fn advance_records_broadcast_position() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance_broadcast("reviewer", v[0]);
        assert_eq!(store.broadcast_position("reviewer"), Some(v[0]));
        store.advance_broadcast("reviewer", v[1]);
        assert_eq!(store.broadcast_position("reviewer"), Some(v[1]));
    }

    #[test]
    fn advance_never_moves_backwards() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance_exclusive(P, v[1]);
        store.advance_exclusive(P, v[0]);
        assert_eq!(store.exclusive_position(P), Some(v[1]), "cursor must not rewind");

        store.advance_broadcast("reviewer", v[1]);
        store.advance_broadcast("reviewer", v[0]);
        assert_eq!(store.broadcast_position("reviewer"), Some(v[1]), "cursor must not rewind");
    }

    #[test]
    fn patterns_are_independent() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance_exclusive("iot_base/dev_01", v[0]);
        store.advance_exclusive("iot_base/planner", v[1]);
        assert_eq!(store.exclusive_position("iot_base/dev_01"), Some(v[0]));
        assert_eq!(store.exclusive_position("iot_base/planner"), Some(v[1]));
    }

    #[test]
    fn broadcast_labels_are_independent() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance_broadcast("reviewer_01", v[0]);
        store.advance_broadcast("reviewer_02", v[1]);
        assert_eq!(store.broadcast_position("reviewer_01"), Some(v[0]));
        assert_eq!(store.broadcast_position("reviewer_02"), Some(v[1]));
    }

    /// A label's broadcast position and a pattern's exclusive position are
    /// independent: advancing one never touches the other.
    #[test]
    fn exclusive_and_broadcast_positions_are_independent() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance_exclusive(P, v[0]);
        store.advance_broadcast("reviewer", v[1]);
        assert_eq!(store.exclusive_position(P), Some(v[0]));
        assert_eq!(store.broadcast_position("reviewer"), Some(v[1]));
    }

    #[test]
    fn snap_forward_moves_stale_cursors_only() {
        let mut store = CursorStore::default();
        let v = ids(3);
        store.advance_exclusive("stale", v[0]);
        store.advance_exclusive("fresh", v[2]);
        store.advance_broadcast("stale_label", v[0]);
        // v[1] is now the oldest surviving message.
        let mut snapped = store.snap_forward(v[1]);
        snapped.sort();
        assert_eq!(snapped, vec!["broadcast:stale_label".to_owned(), "exclusive:stale".to_owned()]);
        assert_eq!(store.exclusive_position("stale"), Some(v[1]));
        assert_eq!(store.exclusive_position("fresh"), Some(v[2]), "fresh cursor untouched");
        assert_eq!(store.broadcast_position("stale_label"), Some(v[1]));
    }

    /// A cursor sitting exactly on the oldest surviving message has already
    /// consumed it and missed nothing, so it must be left alone and must not
    /// be reported in `status` as having lost messages.
    #[test]
    fn snap_forward_leaves_a_cursor_exactly_at_the_boundary() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance_exclusive(P, v[1]);
        let snapped = store.snap_forward(v[1]);
        assert!(snapped.is_empty(), "a cursor at the boundary has not missed anything");
        assert_eq!(store.exclusive_position(P), Some(v[1]));
    }

    #[test]
    fn roundtrips_through_json() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance_exclusive("iot_base/dev_01", v[0]);
        store.advance_broadcast("reviewer", v[1]);
        let json = store.to_json().unwrap();
        let back = CursorStore::from_json(&json).unwrap();
        assert_eq!(back.exclusive_position("iot_base/dev_01"), Some(v[0]));
        assert_eq!(back.broadcast_position("reviewer"), Some(v[1]));
    }

    #[test]
    fn json_shape_has_exclusive_and_broadcast_objects() {
        let mut store = CursorStore::default();
        let v = ids(1);
        store.advance_exclusive("iot_base/**", v[0]);
        store.advance_broadcast("reviewer", v[0]);
        let json = store.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["exclusive"]["iot_base/**"], serde_json::json!(v[0].to_string()));
        assert_eq!(parsed["broadcast"]["reviewer"], serde_json::json!(v[0].to_string()));
    }

    #[test]
    fn from_json_tolerates_empty_input() {
        assert_eq!(CursorStore::from_json("").unwrap().exclusive_position(P), None);
    }

    /// A cursor file from an older single-map format is not convertible, so it
    /// must reset to empty *and* hand back a reason for the caller to log
    /// rather than dropping the positions silently.
    #[test]
    fn old_format_resets_and_reports_a_reason() {
        let old = r#"{"iot_base/**":"01J000000000000000000000"}"#;
        assert!(CursorStore::from_json(old).is_err(), "the old flat format must not parse");

        let (store, reason) = CursorStore::from_json_or_reset(old);
        assert_eq!(store.exclusive_position(P), None);
        assert!(reason.is_some(), "an unreadable cursor file must not be discarded silently");
    }

    #[test]
    fn from_json_or_reset_is_quiet_on_valid_input() {
        let mut store = CursorStore::default();
        let v = ids(1);
        store.advance_exclusive(P, v[0]);
        let (back, reason) = CursorStore::from_json_or_reset(&store.to_json().unwrap());
        assert_eq!(reason, None, "a readable file must not be reported as damaged");
        assert_eq!(back.exclusive_position(P), Some(v[0]));
    }
}
