//! The append-only journal writer + replay (§3.2).
//!
//! The journal is the **source of truth**. Every mutating event is appended
//! (with fsync) before the in-memory state / `SQLite` projection is updated.
//! Records are idempotent (they carry the full new state value), so replay is
//! safe and a truncated tail only loses trailing events. The pure parsing side
//! lives in `supervisor-core::journal`; this module owns the file I/O.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use supervisor_core::journal::{JournalRecord, JournalType, replay};
use supervisor_core::time::now_rfc3339;

/// The append-only JSONL journal under `~/.supervisor/`.
pub struct Journal {
    file: File,
    path: PathBuf,
    /// The next sequence number to assign (max seen + 1 on open).
    next_seq: u64,
}

/// A replay result: well-formed records in order, plus the corrupt lines
/// `(line_number, reason)` that were skipped.
pub type ReplayResult = (Vec<JournalRecord>, Vec<(usize, String)>);

impl Journal {
    /// Open (creating if absent) and fast-forward the sequence counter by
    /// replaying the existing lines. A corrupt tail only loses those lines.
    ///
    /// # Errors
    /// Any I/O failure while opening or creating the journal.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating journal dir {}", parent.display()))?;
        }
        // I-32: journal lines contain pasted secrets (inbox bodies); the file
        // must be 0600, not the default umask 0644.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening journal {}", path.display()))?;
        if let Ok(metadata) = file.metadata() {
            metadata.permissions().set_mode(0o600);
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading journal {}", path.display()))?;
        let next_seq =
            replay(&contents).records.iter().map(|r| r.seq).max().map_or(1, |max| max + 1);
        Ok(Self { file, path: path.to_owned(), next_seq })
    }

    /// The path of the journal file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The next sequence number that will be assigned.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Append an idempotent record (the payload must carry the full new state
    /// value) and fsync it. Returns the recorded event.
    ///
    /// # Errors
    /// I/O failures, surfaced rather than swallowed.
    pub fn append(
        &mut self,
        r#type: JournalType,
        data: serde_json::Value,
    ) -> Result<JournalRecord> {
        let record = JournalRecord { seq: self.next_seq, r#type, data, ts: now_rfc3339() };
        self.next_seq += 1;
        let mut line = record.to_line();
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .with_context(|| format!("appending to journal {}", self.path.display()))?;
        self.file.sync_all().with_context(|| format!("fsync journal {}", self.path.display()))?;
        Ok(record)
    }

    /// Replay the journal from disk, returning every well-formed record in
    /// order and the corrupt lines that were skipped.
    ///
    /// # Errors
    /// I/O failures while reading.
    pub fn replay_file(&self) -> Result<ReplayResult> {
        let contents = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading journal {}", self.path.display()))?;
        let replay = replay(&contents);
        Ok((replay.records, replay.skipped))
    }

    /// Rebuild the journal from a full record list (used when the projection
    /// is being rebuilt / the file was truncated by an external tool).
    ///
    /// # Errors
    /// I/O failures.
    pub fn rewrite(&mut self, records: &[JournalRecord]) -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)
            .with_context(|| format!("rewriting journal {}", self.path.display()))?;
        for record in records {
            let mut line = record.to_line();
            line.push('\n');
            file.write_all(line.as_bytes())?;
        }
        file.sync_all()?;
        self.next_seq = records.iter().map(|r| r.seq).max().map_or(1, |m| m + 1);
        self.file = file;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use supervisor_core::journal::JournalType;
    #[test]
    fn open_assigns_sequence_from_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(journal.next_seq(), 1);
        let _ = journal.append(JournalType::PortAlloc, serde_json::json!({"port": 4101}));
        let _ = journal.append(JournalType::PortAlloc, serde_json::json!({"port": 4102}));
        drop(journal);

        let reopened = Journal::open(&path).unwrap();
        assert_eq!(reopened.next_seq(), 3, "sequence continues past replayed records");
        let (records, skipped) = reopened.replay_file().unwrap();
        assert_eq!(records.len(), 2);
        assert!(skipped.is_empty());
    }

    #[test]
    fn append_fsyncs_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        let record = journal
            .append(
                JournalType::AgentState,
                serde_json::json!({"agent_id": "dev_01", "state": "idle"}),
            )
            .unwrap();
        assert_eq!(record.seq, 1);
        let (records, _) = journal.replay_file().unwrap();
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[0].r#type, JournalType::AgentState);
        assert_eq!(records[0].data["agent_id"], "dev_01");
    }

    #[test]
    fn corrupt_tail_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        let _ = journal.append(JournalType::PortAlloc, serde_json::json!({"port": 1}));
        drop(journal);
        // Simulate a truncated/corrupt tail.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"this is not json\n").unwrap();
        drop(file);

        let reopened = Journal::open(&path).unwrap();
        let (records, skipped) = reopened.replay_file().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(skipped.len(), 1);
        assert_eq!(reopened.next_seq(), 2, "corrupt line does not advance the sequence");
    }

    #[test]
    fn rewrite_rebuilds_and_advances_seq() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        let r1 = journal.append(JournalType::PortAlloc, serde_json::json!({"port": 1})).unwrap();
        let r2 = journal.append(JournalType::PortAlloc, serde_json::json!({"port": 2})).unwrap();
        journal.rewrite(&[r1, r2]).unwrap();
        assert_eq!(journal.next_seq(), 3);
        let (records, _) = journal.replay_file().unwrap();
        assert_eq!(records.len(), 2);
    }
}
