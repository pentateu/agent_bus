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
    CursorStore, Message, Pattern, RetentionPolicy,
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
    pub cursor: String,
    /// Messages behind this cursor, across all topics in the partition.
    pub lag: usize,
    /// True if pruning dragged this cursor forward, meaning the subscriber
    /// provably missed messages.
    pub snapped: bool,
}

/// One isolated project namespace: its log, its cursors, its waiters.
pub struct Partition {
    name: String,
    log: PartitionLog,
    cursors: CursorStore,
    cursor_file: PathBuf,
    /// Subscribers whose cursors were dragged forward by pruning, meaning they
    /// provably missed messages. Reported by `status`.
    ///
    /// Deliberately in-memory only: it describes what this daemon process
    /// observed, and a restart genuinely cannot know whether an old cursor was
    /// snapped or simply acknowledged.
    snapped: Vec<String>,
    notify: broadcast::Sender<()>,
}

impl Partition {
    /// Open a partition, loading its log and cursors from disk.
    ///
    /// # Errors
    /// Returns an error if the log or cursor file cannot be read or parsed.
    pub fn open(state_dir: &Path, name: &str) -> Result<Self> {
        let log = PartitionLog::open(&log_path(state_dir, name))?;
        let cursor_file = cursor_path(state_dir, name);
        let cursors = match std::fs::read_to_string(&cursor_file) {
            Ok(raw) => CursorStore::from_json(&raw)
                .with_context(|| format!("parsing {}", cursor_file.display()))?,
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

        Ok(Self { name: name.to_owned(), log, cursors, cursor_file, snapped: Vec::new(), notify })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
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

    /// Messages matching `pattern` that `subscriber` has not consumed.
    ///
    /// Borrows rather than clones: most callers only count or inspect the
    /// result, and the handler clones just the messages it actually sends.
    #[must_use]
    pub fn unread(&self, pattern: &Pattern, subscriber: &str) -> Vec<&Message> {
        let cursor = self.cursors.position(subscriber);
        self.log.messages_after(cursor).filter(|m| pattern.matches(&m.topic)).collect()
    }

    /// Replay a time window, ignoring cursors entirely.
    #[must_use]
    pub fn history(&self, pattern: &Pattern, since_secs: Option<u64>, now: u64) -> Vec<&Message> {
        self.log.messages_since(since_secs, now).filter(|m| pattern.matches(&m.topic)).collect()
    }

    /// Record that `subscriber` consumed up to `id`, and persist it.
    ///
    /// # Errors
    /// Returns an error if the cursor file cannot be written.
    pub fn acknowledge(&mut self, subscriber: &str, id: Ulid) -> Result<()> {
        self.cursors.advance(subscriber, id);
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
                for subscriber in snapped {
                    if !self.snapped.contains(&subscriber) {
                        self.snapped.push(subscriber);
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

    /// Per-subscriber detail for `status`.
    #[must_use]
    pub fn subscriber_snapshots(&self) -> Vec<SubscriberSnapshot> {
        self.cursors
            .subscribers()
            .map(|(id, cursor)| SubscriberSnapshot {
                lag: self.log.messages_after(Some(cursor)).count(),
                snapped: self.snapped.iter().any(|s| s == id),
                id: id.to_owned(),
                cursor: cursor.to_string(),
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
        Partition::open(dir.path(), "iot_base").unwrap()
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
        p.acknowledge("reviewer", m.id).unwrap();
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
        p.acknowledge("reviewer", m.id).unwrap();

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
            p.acknowledge("reviewer", m.id).unwrap();
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
        p.acknowledge("reviewer", m.id).unwrap();

        let pattern = Pattern::parse("iot_base/**").unwrap();
        let now = m.id.timestamp_ms() / 1000;
        assert_eq!(p.history(&pattern, None, now).len(), 1, "history replays consumed messages");
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
