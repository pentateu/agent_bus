//! One partition: its durable log, its per-pattern delivery positions, and its
//! waiter wake-ups.
//!
//! A partition is the hard isolation boundary between unrelated projects
//! sharing one daemon. Nothing here can read another partition's state,
//! because a `Partition` only ever holds paths derived from its own name.
//!
//! Delivery is exclusive per pattern: [`Partition::deliver`] computes the
//! unread messages matching a pattern and advances that pattern's position in
//! the same step, under the caller's lock. Because every delivery runs inside
//! the daemon's single state lock, no other client can interleave — so exactly
//! one consumer ever receives each message. There is no separate
//! acknowledge-and-later-advance step to race.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use agent_bus_core::{
    CursorStore, Message, PartitionName, Pattern, Priority, RetentionPolicy,
    paths::{cursor_path, log_path},
};
use anyhow::{Context, Result};
use tokio::sync::broadcast;
use ulid::Ulid;

use crate::log::PartitionLog;
use crate::metrics::PartitionTally;

/// Capacity of the wake-up channel. Waiters re-scan the log on any signal, so a
/// lagged receiver is harmless — it just means "check again".
const NOTIFY_CAPACITY: usize = 64;

/// One position's line in `status`.
///
/// A position is either an exclusive per-pattern position or a broadcast
/// per-label position; `broadcast` says which, so the two never collide even
/// though both report a `key` and a `label`.
pub struct PatternSnapshot {
    /// The position key: a pattern string for exclusive delivery, a consumer
    /// label for broadcast delivery.
    pub key: String,
    /// The `--as` label most recently used with this pattern, if any. For a
    /// broadcast position this is the same as `key` — the label IS the
    /// position's identity.
    pub label: String,
    /// True when this is a broadcast (per-label) position rather than an
    /// exclusive (per-pattern) position.
    pub broadcast: bool,
    pub cursor: String,
    /// Messages behind this cursor that match its pattern.
    pub lag: usize,
    /// True if pruning dragged this cursor forward, meaning messages were lost.
    pub snapped: bool,
}

/// One isolated project namespace: its log, its per-pattern positions, its
/// waiters.
pub struct Partition {
    name: PartitionName,
    log: PartitionLog,
    cursors: CursorStore,
    cursor_file: PathBuf,
    /// Pattern strings whose cursors were dragged forward by pruning, meaning
    /// messages were provably lost.
    ///
    /// Deliberately in-memory only: it describes what this daemon process
    /// observed, and a restart genuinely cannot know whether an old cursor was
    /// snapped or simply acknowledged.
    snapped: Vec<String>,
    /// The most recent `--as` label seen per pattern. In-memory only: a label
    /// is cosmetic, and persisting it would let a stale name outlive the
    /// process that chose it.
    labels: std::collections::BTreeMap<String, String>,
    /// Cumulative per-partition counters for the dashboard. Bumped inside
    /// `publish` / `deliver` / `prune`, which already run under the state lock.
    tally: PartitionTally,
    notify: broadcast::Sender<()>,
}

impl Partition {
    /// Open a partition, loading its log and cursors from disk.
    ///
    /// Takes an already-validated [`PartitionName`], so a client-supplied
    /// string cannot reach the filesystem without passing
    /// [`PartitionName::parse`] first.
    ///
    /// # Errors
    /// Returns an error if the log cannot be read or the cursor file cannot be
    /// read for a reason other than being absent or unparseable.
    pub fn open(state_dir: &Path, name: &PartitionName) -> Result<Self> {
        let log = PartitionLog::open(&log_path(state_dir, name))?;
        let cursor_file = cursor_path(state_dir, name);
        let cursors = match std::fs::read_to_string(&cursor_file) {
            Ok(raw) => {
                // An unreadable cursor file resets rather than failing the
                // open, so one bad file cannot make the partition unusable.
                // Reported on stderr because losing read positions silently is
                // exactly the failure this must not become; see
                // `CursorStore::from_json_or_reset` for why resetting is the
                // right call for a file that is stale within the hour.
                let (store, reason) = CursorStore::from_json_or_reset(&raw);
                if let Some(reason) = reason {
                    eprintln!(
                        "agent-bus: ignoring unreadable cursor file {} ({reason}); \
                         unread messages in {name} will be delivered again",
                        cursor_file.display()
                    );
                    store
                } else if let Some(triples) = CursorStore::legacy_triples(&raw) {
                    // A legacy cursor file migrated into per-pattern positions.
                    // Seed the per-label delivered set from the log so a label
                    // that already consumed a message is not handed it again by
                    // an overlapping pattern after the upgrade.
                    eprintln!(
                        "agent-bus: migrating legacy cursor file {} into per-pattern positions",
                        cursor_file.display()
                    );
                    let mut store = store;
                    for (subscriber, pattern, id) in triples {
                        let Ok(pattern) = Pattern::parse(&pattern) else { continue };
                        for message in log.messages() {
                            if message.id <= id && pattern.matches(&message.topic) {
                                store.mark_delivered(&subscriber, message.id);
                            }
                        }
                    }
                    store
                } else {
                    store
                }
            }
            // A missing cursor file is the normal first-run state, not an
            // error: every pattern simply starts at "nothing consumed".
            // Checked by kind rather than by a prior `exists()` call so a file
            // deleted between the two cannot turn into a spurious failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CursorStore::default(),
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", cursor_file.display()));
            }
        };
        let (notify, _) = broadcast::channel(NOTIFY_CAPACITY);

        Ok(Self {
            name: name.clone(),
            log,
            cursors,
            cursor_file,
            snapped: Vec::new(),
            labels: std::collections::BTreeMap::new(),
            tally: PartitionTally::new(),
            notify,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    #[must_use]
    pub fn tally(&self) -> &PartitionTally {
        &self.tally
    }

    /// Every retained message, in id order. Used by `build_metrics` to derive
    /// publishers and topic counts from what the log actually holds.
    pub fn log_messages(&self) -> impl Iterator<Item = &Message> {
        self.log.messages().iter()
    }

    /// Receive a signal whenever a message is published here.
    #[must_use]
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<()> {
        self.notify.subscribe()
    }

    /// Durably append a message and wake any waiters.
    ///
    /// # Errors
    /// Returns an error if the append fails. The message is not considered
    /// published unless it reached disk.
    #[allow(clippy::needless_pass_by_value)]
    pub fn publish(&mut self, message: Message) -> Result<Ulid> {
        let id = message.id;
        let body_bytes = u64::try_from(message.body.len()).unwrap_or(u64::MAX);
        self.tally.record_post(body_bytes, message.priority == Priority::High, message.broadcast);
        self.log.append(&message)?;
        // Signalled only after the append succeeded, so a woken waiter always
        // finds the message on its re-scan. An error here only means nobody is
        // listening, which is the normal case.
        let _ = self.notify.send(());
        Ok(id)
    }

    /// Deliver up to `limit` messages matching `pattern` and advance positions.
    ///
    /// Two kinds of messages are delivered, and each has its own position:
    ///
    /// * **Exclusive** messages (`broadcast == false`) are first-consumer-wins
    ///   per pattern. The advance happens here, in the same locked call as the
    ///   scan, so two consumers racing on one pattern cannot both receive the
    ///   same message: whichever takes the lock first delivers and advances;
    ///   the other then sees nothing unread. A later `read`/`wait`/`follow`
    ///   under that pattern will not return it again.
    ///
    /// * **Broadcast** messages (`broadcast == true`) are delivered once per
    ///   consumer label. Each label's broadcast position advances past what
    ///   this consumer has seen, so every distinct `--as` label gets its own
    ///   copy — and only one copy, no matter how many patterns it reads that
    ///   happen to match.
    ///
    /// The per-label delivered set closes the overlap the positions cannot:
    /// if a label reads both `iot_base/dev_01` and `iot_base/*`, a message
    /// matching both would otherwise be delivered twice to that label. Any
    /// message id the label has already received is filtered out before the
    /// pattern positions are consulted.
    ///
    /// `limit` lets `wait` deliver at most one unit of work while `read` and
    /// `follow` drain everything currently unread. `history` is the only way to
    /// see a delivered message again.
    ///
    /// # Errors
    /// Returns an error if a cursor position cannot be persisted.
    pub fn deliver(
        &mut self,
        pattern: &Pattern,
        label: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let exclusive_cursor = self.cursors.exclusive_position(pattern.as_str());
        let broadcast_cursor = self.cursors.broadcast_position(label);

        // Merge the two streams in id order and take `limit`. Each stream is
        // already sorted, so this is a merge of two sorted lists rather than a
        // full sort. Already-delivered ids are dropped first, so overlapping
        // patterns cannot hand the same message to this label twice.
        let exclusive: Vec<Message> = self
            .log
            .messages_after(exclusive_cursor)
            .filter(|m| {
                !m.broadcast && !self.cursors.is_delivered(label, m.id) && pattern.matches(&m.topic)
            })
            .take(limit)
            .cloned()
            .collect();
        let broadcast: Vec<Message> = self
            .log
            .messages_after(broadcast_cursor)
            .filter(|m| {
                m.broadcast && !self.cursors.is_delivered(label, m.id) && pattern.matches(&m.topic)
            })
            .take(limit)
            .cloned()
            .collect();

        let mut merged = Vec::with_capacity(exclusive.len() + broadcast.len());
        let mut e = exclusive.into_iter();
        let mut b = broadcast.into_iter();
        let mut e_next = e.next();
        let mut b_next = b.next();
        while let (Some(e_msg), Some(b_msg)) = (e_next.as_ref(), b_next.as_ref()) {
            if e_msg.id < b_msg.id {
                if let Some(taken) = e_next.take() {
                    merged.push(taken);
                }
                e_next = e.next();
            } else {
                if let Some(taken) = b_next.take() {
                    merged.push(taken);
                }
                b_next = b.next();
            }
        }
        // One stream is exhausted. Drain the survivor: the item already pulled
        // (`e_next`/`b_next`) plus everything still in the iterator.
        merged.extend(e_next.into_iter().chain(e));
        merged.extend(b_next.into_iter().chain(b));
        merged.truncate(limit);

        // Advance each position to the last delivered message of its kind, and
        // record every delivered id against the label so no overlapping pattern
        // can deliver the same message again.
        if let Some(last) = merged.iter().rev().find(|m| !m.broadcast) {
            self.cursors.advance_exclusive(pattern.as_str(), last.id);
        }
        if let Some(last) = merged.iter().rev().find(|m| m.broadcast) {
            self.cursors.advance_broadcast(label, last.id);
        }
        if !merged.is_empty() {
            for message in &merged {
                self.cursors.mark_delivered(label, message.id);
            }
            self.persist_cursors()?;
        }
        self.labels.insert(pattern.as_str().to_owned(), label.to_owned());
        let delivered = u64::try_from(merged.len()).unwrap_or(u64::MAX);
        self.tally.record_deliveries(delivered);
        Ok(merged)
    }

    /// Replay a time window, ignoring cursors entirely.
    #[must_use]
    pub fn history(&self, pattern: &Pattern, since_secs: Option<u64>, now: u64) -> Vec<&Message> {
        self.log.messages_since(since_secs, now).filter(|m| pattern.matches(&m.topic)).collect()
    }

    /// Apply retention and drag stale cursors forward.
    ///
    /// # Errors
    /// Returns an error if the log rewrite or cursor write fails.
    #[allow(clippy::trivially_copy_pass_by_ref)] // Matches `PartitionLog::prune`.
    pub fn prune(&mut self, policy: &RetentionPolicy, now: u64) -> Result<usize> {
        let outcome = self.log.prune(policy, now)?;
        let mut snapped_count: u64 = 0;
        if outcome.removed > 0
            && let Some(oldest) = outcome.oldest_surviving
        {
            // `snap_forward` snaps a stale cursor *to* `oldest_surviving`,
            // which marks that message as already delivered, so the pattern or
            // label is not handed it again. That is the right call: the cursor
            // pointed into history that has now been deleted, so messages have
            // demonstrably been lost either way and the delivery gap is real,
            // not something one extra message repairs. Losing them and
            // reporting `snapped: true` in `status` makes the gap visible
            // rather than papering over it with a partial replay.
            let snapped_keys = self.cursors.snap_forward(oldest);
            snapped_count = u64::try_from(snapped_keys.len()).unwrap_or(u64::MAX);
            for key in snapped_keys {
                if !self.snapped.contains(&key) {
                    self.snapped.push(key);
                }
            }
            // Delivered records for pruned messages are pure memory; drop them
            // so the per-label sets do not grow without bound.
            self.cursors.prune_delivered(oldest);
            self.persist_cursors()?;
        }
        self.tally.record_prune(u64::try_from(outcome.removed).unwrap_or(u64::MAX), snapped_count);
        Ok(outcome.removed)
    }

    #[must_use]
    pub fn message_count(&self) -> usize {
        self.log.messages().len()
    }

    #[must_use]
    pub fn skipped_records(&self) -> usize {
        self.log.skipped_records()
    }

    #[must_use]
    pub fn oldest_age_secs(&self, now: u64) -> Option<u64> {
        self.log.messages().first().map(|m| m.age_secs(now))
    }

    /// Per-position detail for `status`.
    ///
    /// One line per exclusive pattern position, then one per broadcast label
    /// position. Lag counts only messages the position's own kind and pattern
    /// select: exclusive lag counts non-broadcast messages a pattern has not
    /// delivered, broadcast lag counts broadcast messages a label has not seen.
    #[must_use]
    pub fn pattern_snapshots(&self) -> Vec<PatternSnapshot> {
        let mut snapshots = Vec::new();

        for (pattern, cursor) in self.cursors.exclusive_cursors() {
            // A stored pattern that no longer parses cannot select anything, so
            // it contributes no lag rather than failing the whole status call.
            let lag = Pattern::parse(pattern).map_or(0, |parsed| {
                self.log
                    .messages_after(Some(cursor))
                    .filter(|m| !m.broadcast && parsed.matches(&m.topic))
                    .count()
            });
            let label = self.labels.get(pattern).cloned().unwrap_or_default();
            snapshots.push(PatternSnapshot {
                lag,
                snapped: self.snapped.iter().any(|s| s == &format!("exclusive:{pattern}")),
                label,
                key: pattern.to_owned(),
                broadcast: false,
                cursor: cursor.to_string(),
            });
        }

        for (label, cursor) in self.cursors.broadcast_cursors() {
            let lag = self.log.messages_after(Some(cursor)).filter(|m| m.broadcast).count();
            snapshots.push(PatternSnapshot {
                lag,
                snapped: self.snapped.iter().any(|s| s == &format!("broadcast:{label}")),
                label: label.to_owned(),
                key: label.to_owned(),
                broadcast: true,
                cursor: cursor.to_string(),
            });
        }

        snapshots
    }

    /// Write the cursor file via a temp file and a rename, so a reader never
    /// sees a half-written file.
    ///
    /// Deliberately not fsynced, unlike the log. The asymmetry is intentional:
    /// a cursor lost to a power failure redelivers messages the pattern already
    /// saw, which exclusive delivery absorbs cleanly. A *message* lost the same
    /// way is unrecoverable, so only the log pays for an fsync on every write.
    fn persist_cursors(&self) -> Result<()> {
        let json = self.cursors.to_json().context("serializing cursors")?;
        let temp = self.temp_cursor_path();
        std::fs::write(&temp, &json).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, &self.cursor_file)
            .with_context(|| format!("replacing {}", self.cursor_file.display()))?;
        Ok(())
    }

    /// Scratch path for a cursor rewrite.
    ///
    /// Appends to the whole file name rather than using
    /// `Path::with_extension`, which replaces only the final component and so
    /// works here purely by coincidence of the `.cursors.json` naming. The pid
    /// keeps concurrent daemons pointed at the same state directory from
    /// clobbering each other's scratch file. Mirrors `PartitionLog::temp_path`.
    fn temp_cursor_path(&self) -> PathBuf {
        let mut name = self.cursor_file.file_name().map_or_else(OsString::new, OsStr::to_os_string);
        name.push(format!(".{}.tmp", std::process::id()));
        self.cursor_file.with_file_name(name)
    }
}

#[cfg(test)]
mod tests {
    use agent_bus_core::{Message, Priority, Topic};
    use tempfile::TempDir;

    use super::*;

    fn msg(topic: &str, body: &str) -> Message {
        Message::new(Topic::parse(topic).unwrap(), body.to_owned(), Priority::Normal, None)
    }

    fn partition(dir: &TempDir) -> Partition {
        Partition::open(dir.path(), &PartitionName::parse("iot_base").unwrap()).unwrap()
    }

    #[test]
    fn unread_returns_matching_messages_only() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "from dev")).unwrap();
        p.publish(msg("iot_base/planner", "from planner")).unwrap();

        let pattern = Pattern::parse("iot_base/dev_01").unwrap();
        let delivered: Vec<String> =
            p.deliver(&pattern, "dev", usize::MAX).unwrap().into_iter().map(|m| m.body).collect();
        assert_eq!(delivered, vec!["from dev".to_owned()]);
    }

    #[test]
    fn delivered_messages_are_not_returned_again() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "one")).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        assert_eq!(p.deliver(&pattern, "r", usize::MAX).unwrap().len(), 1);
        assert!(
            p.deliver(&pattern, "r", usize::MAX).unwrap().is_empty(),
            "delivery must be exclusive: once handed out, never returned"
        );
    }

    /// Delivery exclusivity is per pattern: reading one pattern advances only
    /// that pattern's position, leaving another pattern's position untouched.
    #[test]
    fn delivering_one_pattern_does_not_consume_another_patterns_messages() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);

        // `planner` is published first, so a partition-wide position set to the
        // `dev_01` id would seek straight past it.
        p.publish(msg("iot_base/planner", "P1")).unwrap();
        let dev = msg("iot_base/dev_01", "D1");
        p.publish(dev.clone()).unwrap();

        let dev_pattern = Pattern::parse("iot_base/dev_01").unwrap();
        let planner_pattern = Pattern::parse("iot_base/planner").unwrap();

        p.deliver(&dev_pattern, "w1", usize::MAX).unwrap();

        let planner: Vec<String> = p
            .deliver(&planner_pattern, "w1", usize::MAX)
            .unwrap()
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(
            planner,
            vec!["P1".to_owned()],
            "the planner message must survive a delivery on dev_01"
        );
    }

    /// Two consumers of the same pattern share one position, so the first to
    /// deliver takes the message and the second sees nothing.
    #[test]
    fn concurrent_consumers_of_one_pattern_are_exclusive() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "one")).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        assert_eq!(p.deliver(&pattern, "reviewer_01", usize::MAX).unwrap().len(), 1);
        assert_eq!(
            p.deliver(&pattern, "reviewer_02", usize::MAX).unwrap().len(),
            0,
            "a second consumer under the same pattern must not receive the message"
        );
    }

    #[test]
    fn wait_delivers_at_most_one_message() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        for body in ["one", "two", "three"] {
            p.publish(msg("iot_base/dev_01", body)).unwrap();
        }

        let pattern = Pattern::parse("iot_base/**").unwrap();
        let first = p.deliver(&pattern, "w", 1).unwrap();
        assert_eq!(first.len(), 1, "wait delivers exactly one, oldest first");
        assert_eq!(first[0].body, "one");

        let rest = p.deliver(&pattern, "w", 1).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].body, "two");
    }

    #[test]
    fn history_ignores_cursors() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        let m = msg("iot_base/dev_01", "one");
        p.publish(m.clone()).unwrap();
        p.deliver(&Pattern::parse("iot_base/**").unwrap(), "r", usize::MAX).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        let now = m.id.timestamp_ms() / 1000;
        assert_eq!(p.history(&pattern, None, now).len(), 1, "history replays delivered messages");
    }

    /// An unreadable cursor file must not take the partition down with it: the
    /// daemon resets to empty and carries on, which is what keeps one bad file
    /// from making a project's bus unusable.
    #[test]
    fn an_unreadable_cursor_file_resets_instead_of_failing_the_open() {
        let dir = TempDir::new().unwrap();
        let name = PartitionName::parse("iot_base").unwrap();
        {
            let mut p = partition(&dir);
            p.publish(msg("iot_base/dev_01", "one")).unwrap();
        }
        // Torn, unparseable content: neither the current shape nor any legacy
        // format. This must reset, not fail the open.
        std::fs::write(cursor_path(dir.path(), &name), "{ this is not json").unwrap();

        // Unwrapped rather than `expect`ed: an `Err` here *is* the regression
        // (a bad cursor file failing the open), and the panic reports it.
        let mut p = Partition::open(dir.path(), &name).unwrap();
        assert_eq!(
            p.deliver(&Pattern::parse("iot_base/**").unwrap(), "r", usize::MAX).unwrap().len(),
            1,
            "positions are lost, so the retained log is redelivered rather than dropped"
        );
    }

    /// A legacy nested cursor file must migrate, not reset: an upgrade must not
    /// re-deliver messages the old daemon had already consumed.
    #[test]
    fn a_legacy_cursor_file_migrates_instead_of_resetting() {
        let dir = TempDir::new().unwrap();
        let name = PartitionName::parse("iot_base").unwrap();
        {
            let mut p = partition(&dir);
            let m = msg("iot_base/dev_01", "one");
            p.publish(m.clone()).unwrap();
            // Consume it so its id becomes the cursor position.
            p.deliver(&Pattern::parse("iot_base/dev_01").unwrap(), "dev", usize::MAX).unwrap();
            // Now rewrite the file in the legacy nested shape, pointing at the
            // same consumed id, as an old daemon would have left it.
            let consumed = p
                .pattern_snapshots()
                .iter()
                .find(|s| s.key == "iot_base/dev_01")
                .map(|s| s.cursor.clone())
                .unwrap();
            std::fs::write(
                cursor_path(dir.path(), &name),
                format!(r#"{{"dev":{{"iot_base/dev_01":"{consumed}"}}}}"#),
            )
            .unwrap();
        }

        let mut p = Partition::open(dir.path(), &name).unwrap();
        assert!(
            p.deliver(&Pattern::parse("iot_base/dev_01").unwrap(), "dev", usize::MAX)
                .unwrap()
                .is_empty(),
            "a legacy cursor file must keep the message consumed, not re-deliver it"
        );
        // And the delivered set must be seeded from the legacy file, so an
        // overlapping pattern cannot hand the message back to the same label.
        assert!(
            p.deliver(&Pattern::parse("iot_base/*").unwrap(), "dev", usize::MAX)
                .unwrap()
                .is_empty(),
            "legacy migration must seed the delivered set for overlapping patterns"
        );
    }

    #[test]
    fn publish_notifies_waiters() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        let mut rx = p.subscribe_notifications();
        p.publish(msg("iot_base/dev_01", "wake up")).unwrap();
        assert!(rx.try_recv().is_ok(), "a publish must wake blocked waiters");
    }

    #[test]
    fn label_is_recorded_for_status_but_affects_nothing() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "one")).unwrap();
        let pattern = Pattern::parse("iot_base/**").unwrap();
        p.deliver(&pattern, "reviewer_01", usize::MAX).unwrap();

        let snapshots = p.pattern_snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].label, "reviewer_01");
        assert_eq!(snapshots[0].key, "iot_base/**");
        assert!(!snapshots[0].broadcast);
    }

    #[test]
    fn broadcast_message_is_delivered_to_each_label_once() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/announce", "bcast").broadcast()).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        let first = p.deliver(&pattern, "reviewer_01", usize::MAX).unwrap();
        assert_eq!(first.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), vec!["bcast"]);

        // A different label gets its own copy.
        let second = p.deliver(&pattern, "reviewer_02", usize::MAX).unwrap();
        assert_eq!(second.len(), 1, "a distinct label must get its own broadcast copy");

        // The same label never gets it twice.
        let again = p.deliver(&pattern, "reviewer_01", usize::MAX).unwrap();
        assert!(again.is_empty(), "a label must receive a broadcast message exactly once");
    }

    /// A broadcast message interleaved with exclusive ones must be delivered in
    /// id order, once per label for the broadcast and once per pattern for the
    /// exclusive messages.
    #[test]
    fn broadcast_and_exclusive_messages_merge_in_id_order() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "excl1")).unwrap();
        p.publish(msg("iot_base/announce", "bcast").broadcast()).unwrap();
        p.publish(msg("iot_base/dev_01", "excl2")).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        let batch = p.deliver(&pattern, "r1", usize::MAX).unwrap();
        let bodies: Vec<&str> = batch.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, vec!["excl1", "bcast", "excl2"], "delivery must preserve id order");

        // Second label: only the broadcast is left (exclusive messages are gone).
        let rest = p.deliver(&pattern, "r2", usize::MAX).unwrap();
        assert_eq!(rest.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), vec!["bcast"]);
    }

    /// Regression: the same message matches both `iot_base/dev_01` and
    /// `iot_base/*`. One consumer must not receive it twice just because two of
    /// its patterns select the same message — the wait on the exact inbox and
    /// the hook on the wildcard are the same agent.
    #[test]
    fn overlapping_patterns_deliver_the_message_once_per_label() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "verdict")).unwrap();

        let exact = Pattern::parse("iot_base/dev_01").unwrap();
        let wildcard = Pattern::parse("iot_base/*").unwrap();

        // The agent's wait consumes it via the exact inbox...
        assert_eq!(p.deliver(&exact, "dev", usize::MAX).unwrap().len(), 1);

        // ...so the agent's own hook on the wildcard must NOT see it again.
        assert!(
            p.deliver(&wildcard, "dev", usize::MAX).unwrap().is_empty(),
            "one message must be delivered once per label, even under overlapping patterns"
        );

        // But a genuinely different label still sees it via the wildcard: the
        // exclusivity is per label, not global.
        assert_eq!(p.deliver(&wildcard, "opencode-hook", usize::MAX).unwrap().len(), 1);
    }

    /// Regression, same root cause, other order: the wildcard hook wins the
    /// race and the exact-inbox wait must not then re-deliver the message.
    #[test]
    fn overlapping_patterns_deliver_the_message_once_per_label_other_order() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "verdict")).unwrap();

        let exact = Pattern::parse("iot_base/dev_01").unwrap();
        let wildcard = Pattern::parse("iot_base/*").unwrap();

        assert_eq!(p.deliver(&wildcard, "dev", usize::MAX).unwrap().len(), 1);
        assert!(
            p.deliver(&exact, "dev", usize::MAX).unwrap().is_empty(),
            "the exact inbox must not re-deliver what the wildcard already handed to the same label"
        );
    }

    /// The per-label delivered-set must survive a reopen, or the fix would be
    /// lost on every daemon restart — the exact symptom reported in the field.
    #[test]
    fn delivered_once_per_label_survives_a_reopen() {
        let dir = TempDir::new().unwrap();
        {
            let mut p = partition(&dir);
            p.publish(msg("iot_base/dev_01", "verdict")).unwrap();
            p.deliver(&Pattern::parse("iot_base/dev_01").unwrap(), "dev", usize::MAX).unwrap();
        }
        let mut p = partition(&dir);
        let wildcard = Pattern::parse("iot_base/*").unwrap();
        assert!(
            p.deliver(&wildcard, "dev", usize::MAX).unwrap().is_empty(),
            "a restarted daemon must remember that this label already received the message"
        );
    }

    #[test]
    fn publish_increments_partition_tally() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "x")).unwrap();
        p.publish(msg("iot_base/dev_01", "y")).unwrap();
        assert_eq!(p.tally().posts, 2);
        assert_eq!(p.tally().bytes, 2);
        assert_eq!(p.tally().deliveries, 0);
    }

    #[test]
    fn deliver_increments_delivery_count() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "a")).unwrap();
        p.publish(msg("iot_base/dev_01", "b")).unwrap();
        let pattern = Pattern::parse("iot_base/**").unwrap();
        p.deliver(&pattern, "r", usize::MAX).unwrap();
        assert_eq!(p.tally().deliveries, 2);
        // A second deliver finds nothing, so it must not bump again.
        p.deliver(&pattern, "r", usize::MAX).unwrap();
        assert_eq!(p.tally().deliveries, 2);
    }

    #[test]
    fn prune_records_removed_and_snapped_into_tally() {
        // Real-time ULIDs are all born in the same second, so a retention
        // window that removes "old" would also remove "fresh". Craft the ids
        // with explicit, well-separated timestamps so the ages are controlled.
        fn msg_at(topic: &str, body: &str, ms: u64) -> Message {
            let mut m = msg(topic, body);
            m.id = Ulid::from_parts(ms, 0);
            m
        }
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        let old = msg_at("iot_base/dev_01", "old", 2_000);
        p.publish(old).unwrap();
        let middle = msg_at("iot_base/dev_01", "middle", 3_000);
        p.publish(middle).unwrap();
        let fresh = msg_at("iot_base/dev_01", "fresh", 4_000);
        p.publish(fresh).unwrap();

        // Put a stale cursor in place so snap_forward fires: the cursor points
        // at `old`, which sits inside the pruned range.
        let pattern = Pattern::parse("iot_base/**").unwrap();
        p.deliver(&pattern, "r", 1).unwrap(); // consumes "old", sets cursor
        // 1s retention at `now` (4s) removes `old` (age 2) and `middle` (age 1)
        // but retains `fresh` (age 0), so `oldest_surviving` exists for the snap.
        p.prune(&RetentionPolicy { max_age_secs: 1 }, 4).unwrap();
        assert_eq!(p.tally().pruned, 2);
        // The cursor pointed at `old`, which is pruned, so it gets snapped.
        assert_eq!(p.tally().snapped, 1);
    }
}
