//! The durable, append-only JSONL log for a single partition.
//!
//! A message may be posted long before any reader connects, so delivery cannot
//! rely on a live channel: records go to disk and are read back on demand.

use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use agent_bus_core::{Message, RetentionPolicy};
use anyhow::{Context, Result};
use ulid::Ulid;

/// What a prune pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneOutcome {
    pub removed: usize,
    /// Oldest id still present, used to snap stale cursors forward.
    pub oldest_surviving: Option<Ulid>,
}

/// An append-only JSONL log for one partition, plus an in-memory mirror.
///
/// The file is the source of truth; the vector is a cache rebuilt on open.
/// Messages are kept sorted by id, which holds naturally because ULIDs are
/// monotonic and appends are serialized through the daemon.
pub struct PartitionLog {
    path: PathBuf,
    file: File,
    messages: Vec<Message>,
    skipped_records: usize,
}

impl PartitionLog {
    /// Open (creating if absent) and load existing records.
    ///
    /// Corrupt lines are skipped and counted rather than aborting the load: one
    /// bad line from a partial write must not make the whole partition
    /// unreadable.
    ///
    /// # Errors
    /// Returns an error if the file cannot be created, opened, or read.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state directory {}", parent.display()))?;
        }

        // `append(true)` positions every write at end-of-file at write time,
        // regardless of where the descriptor's offset sits after opening, so
        // the separate read handle below cannot disturb appends on any
        // platform.
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening partition log {}", path.display()))?;

        let mut messages = Vec::new();
        let mut skipped_records = 0usize;
        let reader = BufReader::new(
            File::open(path)
                .with_context(|| format!("reading partition log {}", path.display()))?,
        );
        for (index, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("reading {}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            match Message::from_jsonl(&line) {
                Ok(message) => messages.push(message),
                Err(error) => {
                    skipped_records += 1;
                    // `index` counts every physical line including blanks, so
                    // this really is the line number in the file. The parse
                    // error is included: a skipped record is a real anomaly and
                    // must not be reported as a bare count with no cause.
                    eprintln!(
                        "agent-bus: skipping corrupt record in {} (line {}): {error}",
                        path.display(),
                        index + 1
                    );
                }
            }
        }
        messages.sort_by_key(|m| m.id);

        Ok(Self { path: path.to_path_buf(), file, messages, skipped_records })
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    #[must_use]
    pub fn skipped_records(&self) -> usize {
        self.skipped_records
    }

    /// Append durably. Returns only once the record has reached disk, so a
    /// `post` that exits 0 is a promise the message survives a crash.
    ///
    /// # Errors
    /// Returns an error if serialization, writing, or `fsync` fails.
    pub fn append(&mut self, message: &Message) -> Result<()> {
        let line = message.to_jsonl().context("serializing message")?;
        writeln!(self.file, "{line}")
            .with_context(|| format!("appending to {}", self.path.display()))?;
        self.file.sync_data().with_context(|| format!("fsync on {}", self.path.display()))?;
        self.messages.push(message.clone());
        Ok(())
    }

    /// Every message strictly newer than `cursor`, or all of them when `None`.
    ///
    /// Binary search, so this stays cheap as the log grows.
    pub fn messages_after(&self, cursor: Option<Ulid>) -> impl Iterator<Item = &Message> {
        let start = cursor.map_or(0, |id| self.messages.partition_point(|m| m.id <= id));
        self.messages[start..].iter()
    }

    /// Messages no older than `since_secs`, ignoring cursors. `None` means the
    /// whole retained log.
    pub fn messages_since(
        &self,
        since_secs: Option<u64>,
        now: u64,
    ) -> impl Iterator<Item = &Message> {
        self.messages
            .iter()
            .filter(move |m| since_secs.is_none_or(|window| m.age_secs(now) <= window))
    }

    /// Drop messages older than the policy allows and rewrite the file.
    ///
    /// Writes to a sibling temp file then renames, so a crash mid-prune leaves
    /// either the old log or the new one, never a truncated one. Note the
    /// rename itself is atomic but the *directory entry* is not fsynced, so a
    /// crash immediately after could still lose the rename and leave the
    /// pre-prune log in place. That is acceptable: the worst case is that
    /// already-expired messages linger until the next prune pass.
    ///
    /// # Errors
    /// Returns an error if the rewrite or rename fails.
    // `RetentionPolicy` is `Copy` and small enough that clippy would rather see
    // it by value, but the policy is long-lived daemon configuration that
    // callers hold and pass around; borrowing keeps the call sites uniform with
    // the rest of the config plumbing and avoids churn when the struct grows.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn prune(&mut self, policy: &RetentionPolicy, now: u64) -> Result<PruneOutcome> {
        let before = self.messages.len();
        let retained: Vec<Message> =
            self.messages.iter().filter(|m| policy.retains(m.age_secs(now))).cloned().collect();
        let removed = before - retained.len();

        if removed > 0 {
            let temp = self.temp_path();
            {
                let mut out =
                    File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
                for message in &retained {
                    let line = message.to_jsonl().context("serializing during prune")?;
                    writeln!(out, "{line}")
                        .with_context(|| format!("writing {}", temp.display()))?;
                }
                out.sync_data().with_context(|| format!("fsync on {}", temp.display()))?;
            }
            std::fs::rename(&temp, &self.path)
                .with_context(|| format!("replacing {}", self.path.display()))?;

            // Reopen the append handle: the old one points at the unlinked inode.
            self.file = OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .open(&self.path)
                .with_context(|| format!("reopening {}", self.path.display()))?;
            self.messages = retained;
        }

        Ok(PruneOutcome { removed, oldest_surviving: self.messages.first().map(|m| m.id) })
    }

    /// Scratch path for a prune rewrite.
    ///
    /// Built by appending to the whole file name rather than with
    /// `Path::with_extension`, which *replaces* the final extension and would
    /// turn `p.jsonl` into `p.jsonl.tmp` only by coincidence of the naming.
    /// The pid keeps the name from colliding with the log of a partition that
    /// is literally named `<something>.jsonl` — topics permit dots, so that is
    /// a reachable name, not a hypothetical one.
    fn temp_path(&self) -> PathBuf {
        let mut name = self.path.file_name().map_or_else(OsString::new, OsStr::to_os_string);
        name.push(format!(".{}.tmp", std::process::id()));
        self.path.with_file_name(name)
    }
}

#[cfg(test)]
mod tests {
    use agent_bus_core::{Message, Priority, RetentionPolicy, Topic};
    use tempfile::TempDir;

    use super::*;

    fn msg(body: &str) -> Message {
        Message::new(
            Topic::parse("iot_base/dev_01").unwrap(),
            body.to_owned(),
            Priority::Normal,
            None,
        )
    }

    #[test]
    fn append_then_load_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("iot_base.jsonl");

        let mut log = PartitionLog::open(&path).unwrap();
        let a = msg("first");
        let b = msg("second");
        log.append(&a).unwrap();
        log.append(&b).unwrap();

        let reopened = PartitionLog::open(&path).unwrap();
        let bodies: Vec<&str> = reopened.messages().iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, vec!["first", "second"]);
        assert_eq!(reopened.skipped_records(), 0);
    }

    #[test]
    fn open_creates_a_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("brand_new.jsonl");
        let log = PartitionLog::open(&path).unwrap();
        assert!(log.messages().is_empty());
        assert!(path.exists(), "opening must create the file so later appends succeed");
    }

    #[test]
    fn corrupt_lines_are_skipped_and_counted_not_fatal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("iot_base.jsonl");
        let good = msg("good").to_jsonl().unwrap();
        std::fs::write(&path, format!("{good}\nthis is not json\n\n")).unwrap();

        let log = PartitionLog::open(&path).unwrap();
        assert_eq!(log.messages().len(), 1, "the valid record must still load");
        assert_eq!(log.skipped_records(), 1, "corruption is reported, not swallowed");
    }

    #[test]
    fn messages_after_returns_only_newer_ids() {
        let dir = TempDir::new().unwrap();
        let mut log = PartitionLog::open(&dir.path().join("p.jsonl")).unwrap();
        let a = msg("a");
        let b = msg("b");
        let c = msg("c");
        for m in [&a, &b, &c] {
            log.append(m).unwrap();
        }

        let after_a: Vec<&str> = log.messages_after(Some(a.id)).map(|m| m.body.as_str()).collect();
        assert_eq!(after_a, vec!["b", "c"]);

        let all: Vec<&str> = log.messages_after(None).map(|m| m.body.as_str()).collect();
        assert_eq!(all, vec!["a", "b", "c"]);

        let after_c: Vec<&str> = log.messages_after(Some(c.id)).map(|m| m.body.as_str()).collect();
        assert!(after_c.is_empty());
    }

    #[test]
    fn prune_removes_old_messages_and_rewrites_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("p.jsonl");
        let mut log = PartitionLog::open(&path).unwrap();

        let old = msg("old");
        let fresh = msg("fresh");
        log.append(&old).unwrap();
        log.append(&fresh).unwrap();

        // Anchor "now" to the older message and use the default 1-hour window,
        // so this phase cannot depend on whether the two constructor calls
        // straddled a wall-clock second boundary.
        let now = old.id.timestamp_ms() / 1000;
        let outcome = log.prune(&RetentionPolicy::default(), now).unwrap();
        assert_eq!(outcome.removed, 0, "nothing is old enough to prune yet");

        // Advance the clock well past a tiny window: everything ages out.
        let policy = RetentionPolicy { max_age_secs: 1 };
        let now = fresh.id.timestamp_ms() / 1000;
        let outcome = log.prune(&policy, now + 10).unwrap();
        assert_eq!(outcome.removed, 2);
        assert!(log.messages().is_empty());

        let reopened = PartitionLog::open(&path).unwrap();
        assert!(reopened.messages().is_empty(), "prune must persist to disk");
    }

    #[test]
    fn prune_reports_oldest_surviving_id() {
        let dir = TempDir::new().unwrap();
        let mut log = PartitionLog::open(&dir.path().join("p.jsonl")).unwrap();
        let a = msg("a");
        log.append(&a).unwrap();

        let now = a.id.timestamp_ms() / 1000;
        let outcome = log.prune(&RetentionPolicy::default(), now).unwrap();
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.oldest_surviving, Some(a.id));
    }
}
