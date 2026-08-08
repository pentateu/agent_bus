use std::collections::{BTreeMap, BTreeSet};

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
/// * **Delivered** ids are a per-label record of every message that label has
///   actually received. This closes the overlap gap the two position sets
///   cannot: `iot_base/dev_01` and `iot_base/*` are different patterns with
///   different positions, so a message matching both would otherwise be handed
///   to the same label twice — once through each pattern. The delivered set is
///   checked before delivery and pruned with the log, so it never outlives
///   retention.
///
/// The only way to see a delivered message again is `history`, which ignores
/// positions entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CursorStore {
    /// Exclusive per-pattern positions: pattern -> last delivered id.
    exclusive: BTreeMap<String, Ulid>,
    /// Broadcast per-label positions: label -> last delivered id.
    broadcast: BTreeMap<String, Ulid>,
    /// Message ids each label has received, so overlapping patterns cannot
    /// double-deliver to the same consumer.
    #[serde(default)]
    delivered: BTreeMap<String, BTreeSet<Ulid>>,
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

    /// Has this label already received the given message?
    ///
    /// The delivered set is the exclusive side of the overlap guarantee:
    /// a label must not receive one message twice merely because two of its
    /// patterns (`iot_base/dev_01` and `iot_base/*`) both select it. Position
    /// keys cannot express this, because positions are shared per pattern while
    /// delivery is per label — advancing a pattern past a message would hide it
    /// from a *different* label that has not seen it yet.
    #[must_use]
    pub fn is_delivered(&self, label: &str, id: Ulid) -> bool {
        self.delivered.get(label).is_some_and(|ids| ids.contains(&id))
    }

    /// Record that `label` received `id`.
    pub fn mark_delivered(&mut self, label: &str, id: Ulid) {
        self.delivered.entry(label.to_owned()).or_default().insert(id);
    }

    /// Drop delivered records for messages pruned out of the log, so the
    /// per-label sets do not grow without bound as old messages age away.
    ///
    /// A delivered record for a pruned message is pure memory: the message no
    /// longer exists, so it can never be delivered again regardless of what the
    /// set says.
    pub fn prune_delivered(&mut self, oldest_surviving: Ulid) {
        self.delivered.retain(|_, ids| {
            ids.retain(|id| *id >= oldest_surviving);
            !ids.is_empty()
        });
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
    /// Accepts the current `{ exclusive, broadcast, delivered }` shape and
    /// migrates the pre-exclusive nested format `{ subscriber: { pattern: id } }`
    /// so an upgrade never wipes read positions and re-delivers retained
    /// messages. The migration keeps the furthest position each pattern reached
    /// under any subscriber — the subscriber dimension no longer exists, so this
    /// is the closest lossless-enough mapping: messages already consumed stay
    /// consumed, and the risk is a slightly-ahead position, never a duplicate.
    ///
    /// # Errors
    /// Returns [`CoreError::MalformedRecord`] if non-empty input is neither the
    /// current shape nor the legacy nested shape.
    pub fn from_json(input: &str) -> Result<Self, CoreError> {
        if input.trim().is_empty() {
            return Ok(Self::default());
        }
        // Current shape first.
        if let Ok(store) = serde_json::from_str::<Self>(input) {
            return Ok(store);
        }
        // Legacy nested shape: subscriber -> pattern -> id.
        if let Ok(legacy) = serde_json::from_str::<BTreeMap<String, BTreeMap<String, Ulid>>>(input)
        {
            let mut store = Self::default();
            for patterns in legacy.values() {
                for (pattern, id) in patterns {
                    store.advance_exclusive(pattern, *id);
                }
            }
            return Ok(store);
        }
        Err(CoreError::MalformedRecord(
            "not the current or the legacy cursor file format".to_owned(),
        ))
    }

    /// Parse a cursor file, falling back to an empty store on unreadable input.
    ///
    /// Returns the store and, when the input could not be parsed, the reason —
    /// which the caller is expected to log. The reason is handed back rather
    /// than printed here because this crate does no I/O.
    ///
    /// Resetting rather than failing is deliberate. A file that is neither the
    /// current format nor the legacy nested format is likely torn or foreign;
    /// with one-hour retention, the worst outcome of starting fresh is that
    /// unread messages are redelivered — which, under exclusive delivery, means
    /// they are handed out again exactly once. Whereas refusing to open the
    /// partition would take the whole bus down over a file that will be
    /// irrelevant within the hour.
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

    /// Parse a legacy nested cursor file and report whether migration happened.
    ///
    /// `None` means the input was already current-format or empty. A `Some` is
    /// the list of legacy `(subscriber, pattern, id)` triples that were folded
    /// into per-pattern positions. The daemon uses them to seed the per-label
    /// delivered set from its log, so a label that consumed a message under the
    /// old model is not handed it again by an overlapping pattern after the
    /// upgrade.
    #[must_use]
    pub fn legacy_triples(input: &str) -> Option<Vec<(String, String, Ulid)>> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return None;
        }
        // A current-format file is not legacy, even though its inner values are
        // also ULID strings: without this guard, `{"exclusive": {...}}` would be
        // read as the legacy subscriber "exclusive".
        if trimmed.starts_with('{')
            && (trimmed.contains("\"exclusive\"") || trimmed.contains("\"broadcast\""))
        {
            return None;
        }
        let triples: BTreeMap<String, BTreeMap<String, Ulid>> =
            serde_json::from_str(trimmed).ok()?;
        if triples.is_empty() {
            return None;
        }
        Some(
            triples
                .into_iter()
                .flat_map(|(sub, patterns)| {
                    patterns.into_iter().map(move |(pattern, id)| (sub.clone(), pattern, id))
                })
                .collect(),
        )
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

    /// A cursor file from the legacy nested `subscriber -> pattern -> id` format
    /// must migrate into the current exclusive-per-pattern positions, not reset:
    /// an upgrade must never wipe read positions and re-deliver retained
    /// messages. The furthest position each pattern reached under any subscriber
    /// is kept — the subscriber dimension no longer exists.
    #[test]
    fn legacy_nested_format_migrates_instead_of_resetting() {
        let v = ids(3);
        let old = format!(
            r#"{{"reviewer":{{"iot_base/dev_01":"{0}","iot_base/planner":"{1}"}},"dev":{{"iot_base/dev_01":"{2}"}}}}"#,
            v[0], v[1], v[2]
        );
        let store = CursorStore::from_json(&old).unwrap();
        // Furthest id per pattern wins: the dev read dev_01 further than the
        // reviewer did, so the pattern's shared position is the dev's.
        assert_eq!(store.exclusive_position("iot_base/dev_01"), Some(v[2]));
        assert_eq!(store.exclusive_position("iot_base/planner"), Some(v[1]));
        assert!(store.broadcast_cursors().next().is_none());
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

    #[test]
    fn delivered_records_are_per_label() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.mark_delivered("dev", v[0]);
        assert!(store.is_delivered("dev", v[0]));
        assert!(!store.is_delivered("dev", v[1]), "a label only records what it received");
        assert!(!store.is_delivered("opencode-hook", v[0]), "delivered is per label");
    }

    #[test]
    fn delivered_survives_a_roundtrip() {
        let mut store = CursorStore::default();
        let v = ids(2);
        store.mark_delivered("dev", v[0]);
        store.mark_delivered("dev", v[1]);
        let back = CursorStore::from_json(&store.to_json().unwrap()).unwrap();
        assert!(back.is_delivered("dev", v[0]));
        assert!(back.is_delivered("dev", v[1]));
    }

    #[test]
    fn prune_delivered_drops_pruned_message_records() {
        let mut store = CursorStore::default();
        let v = ids(3);
        store.mark_delivered("dev", v[0]);
        store.mark_delivered("dev", v[1]);
        store.mark_delivered("other", v[0]);
        // v[1] is the oldest surviving message; v[0] is gone.
        store.prune_delivered(v[1]);
        assert!(!store.is_delivered("dev", v[0]));
        assert!(store.is_delivered("dev", v[1]));
        assert!(!store.is_delivered("other", v[0]), "empty label sets are dropped");
    }
}
