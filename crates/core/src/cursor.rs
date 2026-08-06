use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::error::CoreError;

/// Per-subscriber read positions within one partition.
///
/// A position is the id of the last message that subscriber consumed
/// successfully, so "unread" means "id strictly greater than this".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CursorStore {
    positions: BTreeMap<String, Ulid>,
}

impl CursorStore {
    #[must_use]
    pub fn position(&self, subscriber: &str) -> Option<Ulid> {
        self.positions.get(subscriber).copied()
    }

    /// Move a subscriber's cursor forward. Never moves it backwards, so a
    /// duplicate or out-of-order acknowledgement cannot cause redelivery.
    pub fn advance(&mut self, subscriber: &str, id: Ulid) {
        self.positions
            .entry(subscriber.to_owned())
            .and_modify(|current| {
                if id > *current {
                    *current = id;
                }
            })
            .or_insert(id);
    }

    /// After pruning, drag any cursor older than `oldest_surviving` forward to
    /// it, and report which subscribers were affected.
    ///
    /// Without this a subscriber whose cursor pointed into pruned history would
    /// be handed the entire surviving log as "unread".
    pub fn snap_forward(&mut self, oldest_surviving: Ulid) -> Vec<String> {
        let mut snapped = Vec::new();
        for (subscriber, position) in &mut self.positions {
            if *position < oldest_surviving {
                *position = oldest_surviving;
                snapped.push(subscriber.clone());
            }
        }
        snapped
    }

    // No `#[must_use]`: `impl Iterator` already carries it, and repeating it
    // trips clippy's `double_must_use`.
    pub fn subscribers(&self) -> impl Iterator<Item = (&str, Ulid)> {
        self.positions.iter().map(|(k, v)| (k.as_str(), *v))
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
    /// Returns [`CoreError::MalformedRecord`] if non-empty input is not valid JSON.
    pub fn from_json(input: &str) -> Result<Self, CoreError> {
        if input.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(input).map_err(|e| CoreError::MalformedRecord(e.to_string()))
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

    #[test]
    fn unknown_subscriber_starts_at_no_position() {
        let store = CursorStore::default();
        assert_eq!(store.position("reviewer"), None);
    }

    #[test]
    fn advance_records_position() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("reviewer", v[0]);
        assert_eq!(store.position("reviewer"), Some(v[0]));
        store.advance("reviewer", v[1]);
        assert_eq!(store.position("reviewer"), Some(v[1]));
    }

    #[test]
    fn advance_never_moves_backwards() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("reviewer", v[1]);
        store.advance("reviewer", v[0]);
        assert_eq!(store.position("reviewer"), Some(v[1]), "cursor must not rewind");
    }

    #[test]
    fn subscribers_are_independent() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("reviewer", v[0]);
        store.advance("planner", v[1]);
        assert_eq!(store.position("reviewer"), Some(v[0]));
        assert_eq!(store.position("planner"), Some(v[1]));
    }

    #[test]
    fn snap_forward_moves_stale_cursors_only() {
        let mut store = CursorStore::default();
        let v = ids(3);
        store.advance("stale", v[0]);
        store.advance("fresh", v[2]);
        // v[1] is now the oldest surviving message.
        let snapped = store.snap_forward(v[1]);
        assert_eq!(snapped, vec!["stale".to_owned()]);
        assert_eq!(store.position("stale"), Some(v[1]));
        assert_eq!(store.position("fresh"), Some(v[2]), "fresh cursor untouched");
    }

    /// A cursor sitting exactly on the oldest surviving message has already
    /// consumed it and missed nothing, so it must be left alone and must not
    /// be reported in `status` as having lost messages.
    #[test]
    fn snap_forward_leaves_a_cursor_exactly_at_the_boundary() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.advance("boundary", v[1]);
        let snapped = store.snap_forward(v[1]);
        assert!(snapped.is_empty(), "a cursor at the boundary has not missed anything");
        assert_eq!(store.position("boundary"), Some(v[1]));
    }

    #[test]
    fn roundtrips_through_json() {
        let mut store = CursorStore::default();
        let v = ids(1);
        store.advance("reviewer", v[0]);
        let json = store.to_json().unwrap();
        let back = CursorStore::from_json(&json).unwrap();
        assert_eq!(back.position("reviewer"), Some(v[0]));
    }

    #[test]
    fn from_json_tolerates_empty_input() {
        assert_eq!(CursorStore::from_json("").unwrap().position("x"), None);
    }
}
