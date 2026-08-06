//! One partition: its durable log, its cursors, and its waiter wake-ups.
//!
//! A partition is the hard isolation boundary between unrelated projects
//! sharing one daemon. Nothing here can read another partition's state,
//! because a `Partition` only ever holds paths derived from its own name.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use agent_bus_core::{
    CursorStore, Message, PartitionName, Pattern, RetentionPolicy,
    paths::{cursor_path, log_path},
};
use anyhow::{Context, Result};
use tokio::sync::broadcast;
use ulid::Ulid;

use crate::log::PartitionLog;

/// Capacity of the wake-up channel. Waiters re-scan the log on any signal, so a
/// lagged receiver is harmless — it just means "check again".
const NOTIFY_CAPACITY: usize = 64;

/// One subscriber's line in `status`.
///
/// A named struct rather than a tuple: `(String, String, usize, bool)` has two
/// same-typed leading fields that are trivial to transpose at the call site.
pub struct SubscriberSnapshot {
    pub id: String,
    /// The pattern this cursor tracks. Cursors are per (subscriber, pattern),
    /// so the id alone does not identify a position.
    pub pattern: String,
    pub cursor: String,
    /// Messages behind this cursor that match its pattern.
    pub lag: usize,
    /// True if pruning dragged this cursor forward, meaning the subscriber
    /// provably missed messages.
    pub snapped: bool,
}

/// One isolated project namespace: its log, its cursors, its waiters.
pub struct Partition {
    name: PartitionName,
    log: PartitionLog,
    cursors: CursorStore,
    cursor_file: PathBuf,
    /// (subscriber, pattern) pairs whose cursors were dragged forward by
    /// pruning, meaning they provably missed messages. Reported by `status`.
    ///
    /// Deliberately in-memory only: it describes what this daemon process
    /// observed, and a restart genuinely cannot know whether an old cursor was
    /// snapped or simply acknowledged.
    snapped: Vec<(String, String)>,
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
                         subscribers in {name} restart from the beginning of the retained log",
                        cursor_file.display()
                    );
                }
                store
            }
            // A missing cursor file is the normal first-run state, not an
            // error: every subscriber simply starts at "nothing consumed".
            // Checked by kind rather than by a prior `exists()` call so a file
            // deleted between the two cannot turn into a spurious failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CursorStore::default(),
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", cursor_file.display()));
            }
        };
        let (notify, _) = broadcast::channel(NOTIFY_CAPACITY);

        Ok(Self { name: name.clone(), log, cursors, cursor_file, snapped: Vec::new(), notify })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.name.as_str()
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
    // Takes the message by value even though the body only borrows it: handing
    // a message to the bus is a transfer of ownership, and callers build one
    // solely to publish it. `PartitionLog::append` cloning internally is an
    // implementation detail that a `&Message` signature would leak into every
    // call site.
    #[allow(clippy::needless_pass_by_value)]
    pub fn publish(&mut self, message: Message) -> Result<Ulid> {
        let id = message.id;
        self.log.append(&message)?;
        // Signalled only after the append succeeded, so a woken waiter always
        // finds the message on its re-scan. An error here only means nobody is
        // listening, which is the normal case.
        let _ = self.notify.send(());
        Ok(id)
    }

    /// Messages matching `pattern` that `subscriber` has not consumed *for that
    /// pattern*.
    ///
    /// The cursor is keyed on the pattern as well as the subscriber, so seeking
    /// past a message this pattern never selected is impossible: a position
    /// only ever reflects messages that were actually delivered under this
    /// pattern.
    ///
    /// Borrows rather than clones: most callers only count or inspect the
    /// result, and the handler clones just the messages it actually sends.
    #[must_use]
    pub fn unread(&self, pattern: &Pattern, subscriber: &str) -> Vec<&Message> {
        let cursor = self.cursors.position(subscriber, pattern.as_str());
        self.log.messages_after(cursor).filter(|m| pattern.matches(&m.topic)).collect()
    }

    /// Replay a time window, ignoring cursors entirely.
    #[must_use]
    pub fn history(&self, pattern: &Pattern, since_secs: Option<u64>, now: u64) -> Vec<&Message> {
        self.log.messages_since(since_secs, now).filter(|m| pattern.matches(&m.topic)).collect()
    }

    /// Record that `subscriber` consumed up to `id` while reading `pattern`,
    /// and persist it.
    ///
    /// The pattern is required because it is half the cursor key: an ack must
    /// advance only the position for the pattern that produced the delivery,
    /// never a sibling pattern's.
    ///
    /// # Errors
    /// Returns an error if the cursor file cannot be written.
    pub fn acknowledge(&mut self, subscriber: &str, pattern: &str, id: Ulid) -> Result<()> {
        self.cursors.advance(subscriber, pattern, id);
        self.persist_cursors()
    }

    /// Apply retention and drag stale cursors forward.
    ///
    /// # Errors
    /// Returns an error if the log rewrite or cursor write fails.
    #[allow(clippy::trivially_copy_pass_by_ref)] // Matches `PartitionLog::prune`.
    pub fn prune(&mut self, policy: &RetentionPolicy, now: u64) -> Result<usize> {
        let outcome = self.log.prune(policy, now)?;
        if outcome.removed > 0
            && let Some(oldest) = outcome.oldest_surviving
        {
            // `snap_forward` snaps a stale cursor *to* `oldest_surviving`,
            // which marks that message as already consumed, so a snapped
            // subscriber is not delivered it. That is the right call: the
            // cursor pointed into history that has now been deleted, so the
            // subscriber has demonstrably missed messages either way and the
            // delivery gap is real, not something one extra message repairs.
            // Snapping to just *before* the boundary would instead hand over
            // one arbitrary survivor while silently dropping the rest, which
            // reads as complete delivery when it is not. Losing it and
            // reporting `snapped: true` in `status` makes the gap visible
            // rather than papering over it with a partial replay.
            let snapped = self.cursors.snap_forward(oldest);
            if !snapped.is_empty() {
                for pair in snapped {
                    if !self.snapped.contains(&pair) {
                        self.snapped.push(pair);
                    }
                }
                self.persist_cursors()?;
            }
        }
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

    /// Per-(subscriber, pattern) detail for `status`.
    ///
    /// One line per cursor, so a subscriber reading two patterns appears twice.
    /// That matches the data model: it has two independent positions and two
    /// independent lags, and collapsing them would have to invent a single
    /// number that describes neither.
    ///
    /// Lag counts only messages the cursor's own pattern selects. Counting
    /// everything after the cursor would report a subscriber as behind on
    /// messages its pattern will never deliver.
    #[must_use]
    pub fn subscriber_snapshots(&self) -> Vec<SubscriberSnapshot> {
        self.cursors
            .subscribers()
            .map(|(id, pattern, cursor)| {
                // A stored pattern that no longer parses cannot select
                // anything, so it contributes no lag rather than failing the
                // whole status call.
                let lag = Pattern::parse(pattern).map_or(0, |parsed| {
                    self.log
                        .messages_after(Some(cursor))
                        .filter(|m| parsed.matches(&m.topic))
                        .count()
                });
                SubscriberSnapshot {
                    lag,
                    snapped: self.snapped.iter().any(|(s, p)| s == id && p == pattern),
                    id: id.to_owned(),
                    pattern: pattern.to_owned(),
                    cursor: cursor.to_string(),
                }
            })
            .collect()
    }

    /// Write the cursor file via a temp file and a rename, so a reader never
    /// sees a half-written file.
    ///
    /// Deliberately not fsynced, unlike the log. The asymmetry is intentional:
    /// a cursor lost to a power failure rewinds a subscriber and redelivers
    /// messages it already saw, which the design already tolerates (`wait` is
    /// at-least-once and clients ack explicitly). A *message* lost the same way
    /// is unrecoverable, so only the log pays for an fsync on every write.
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
    use agent_bus_core::{Message, Pattern, Priority, Topic};
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
        let unread: Vec<&str> =
            p.unread(&pattern, "reviewer").iter().map(|m| m.body.as_str()).collect();
        assert_eq!(unread, vec!["from dev"]);
    }

    #[test]
    fn unread_is_empty_once_acknowledged() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        let m = msg("iot_base/dev_01", "one");
        p.publish(m.clone()).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        assert_eq!(p.unread(&pattern, "reviewer").len(), 1);
        p.acknowledge("reviewer", pattern.as_str(), m.id).unwrap();
        assert!(p.unread(&pattern, "reviewer").is_empty());
    }

    #[test]
    fn a_new_subscriber_sees_the_full_retained_log() {
        // This is the race the durable log exists to solve: the dev agent posts
        // before the reviewer ever connects, and the reviewer must still get it.
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "ready for review")).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        let unread = p.unread(&pattern, "reviewer_that_started_late");
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].body, "ready for review");
    }

    #[test]
    fn cursors_are_per_subscriber() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        let m = msg("iot_base/dev_01", "one");
        p.publish(m.clone()).unwrap();
        p.acknowledge("reviewer", "iot_base/**", m.id).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        assert!(p.unread(&pattern, "reviewer").is_empty());
        assert_eq!(p.unread(&pattern, "planner").len(), 1, "planner has its own cursor");
    }

    #[test]
    fn cursors_survive_reopen() {
        let dir = TempDir::new().unwrap();
        let m = msg("iot_base/dev_01", "one");
        {
            let mut p = partition(&dir);
            p.publish(m.clone()).unwrap();
            p.acknowledge("reviewer", "iot_base/**", m.id).unwrap();
        }
        let p = partition(&dir);
        let pattern = Pattern::parse("iot_base/**").unwrap();
        assert!(p.unread(&pattern, "reviewer").is_empty(), "cursor must persist across restart");
    }

    #[test]
    fn history_ignores_cursors() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        let m = msg("iot_base/dev_01", "one");
        p.publish(m.clone()).unwrap();
        p.acknowledge("reviewer", "iot_base/**", m.id).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        let now = m.id.timestamp_ms() / 1000;
        assert_eq!(p.history(&pattern, None, now).len(), 1, "history replays consumed messages");
    }

    /// Regression: acking a message delivered under one pattern must not
    /// consume the messages a *different* pattern would have delivered to the
    /// same subscriber.
    ///
    /// Previously one cursor per subscriber meant the ack for `dev_01` seeked
    /// past the older `planner` message, which was then unreachable forever.
    #[test]
    fn acking_one_pattern_does_not_consume_another_patterns_messages() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);

        // `planner` is published first, so a subscriber-wide cursor set to the
        // `dev_01` id would seek straight past it.
        p.publish(msg("iot_base/planner", "P1")).unwrap();
        let dev = msg("iot_base/dev_01", "D1");
        p.publish(dev.clone()).unwrap();

        let dev_pattern = Pattern::parse("iot_base/dev_01").unwrap();
        let planner_pattern = Pattern::parse("iot_base/planner").unwrap();

        // The subscriber waits on dev_01 and acks what it was given.
        assert_eq!(p.unread(&dev_pattern, "w1").len(), 1);
        p.acknowledge("w1", dev_pattern.as_str(), dev.id).unwrap();

        let planner: Vec<&str> =
            p.unread(&planner_pattern, "w1").iter().map(|m| m.body.as_str()).collect();
        assert_eq!(planner, vec!["P1"], "the planner message must survive an ack on dev_01");
    }

    /// The same guarantee across a reopen: the per-pattern positions are what
    /// gets persisted, not a single collapsed one.
    #[test]
    fn per_pattern_cursors_survive_reopen() {
        let dir = TempDir::new().unwrap();
        let planner = msg("iot_base/planner", "P1");
        let dev = msg("iot_base/dev_01", "D1");
        {
            let mut p = partition(&dir);
            p.publish(planner.clone()).unwrap();
            p.publish(dev.clone()).unwrap();
            p.acknowledge("w1", "iot_base/dev_01", dev.id).unwrap();
        }

        let p = partition(&dir);
        assert!(
            p.unread(&Pattern::parse("iot_base/dev_01").unwrap(), "w1").is_empty(),
            "the acked pattern stays acked"
        );
        assert_eq!(
            p.unread(&Pattern::parse("iot_base/planner").unwrap(), "w1").len(),
            1,
            "the unacked pattern still has its message after a restart"
        );
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
        // The pre-rekeying flat format, which no longer parses.
        std::fs::write(
            cursor_path(dir.path(), &name),
            r#"{"reviewer":"01J000000000000000000000"}"#,
        )
        .unwrap();

        // Unwrapped rather than `expect`ed: an `Err` here *is* the regression
        // (a bad cursor file failing the open), and the panic reports it.
        let p = Partition::open(dir.path(), &name).unwrap();
        assert_eq!(
            p.unread(&Pattern::parse("iot_base/**").unwrap(), "reviewer").len(),
            1,
            "positions are lost, so the retained log is redelivered rather than dropped"
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
}
