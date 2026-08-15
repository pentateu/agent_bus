//! The journal model (§3.2): the append-only source of truth.
//!
//! Every mutating event is journaled *before* the in-memory state / `SQLite`
//! projection is updated. The journal file is JSONL under `~/.supervisor/`;
//! the daemon appends (with fsync) and replays. This module is the pure part:
//! the record format, the typed event payloads, and replay parsing with
//! corrupt-line accounting.
//!
//! Records are idempotent — they carry the full new state value — so replay is
//! safe and a truncated tail only loses trailing events. If the `SQLite`
//! projection disagrees with the journal, the journal wins (§10).

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::types::{AgentState, InboxEntry, NodeState, PortRow, StoredRule, Workspace};

/// The journaled event kinds (§3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalType {
    WorkspaceState,
    AgentState,
    InboxEnqueue,
    InboxDeliver,
    WorkflowTransition,
    /// M3: a workflow was started (survives restart for `restore()`).
    WorkflowStart,
    /// M10: a decision outcome was recorded.
    DecisionOutcome,
    DecisionRecord,
    RuleMerge,
    PortAlloc,
    PortFree,
    /// C-2 (review): proposals must survive restarts (were written to the DB
    /// without a journal entry and wiped by `Store::rebuild`).
    ProposalRecord,
    /// C-2 (review): intake items must survive restarts.
    IntakeRecord,
    /// C-2 (review): usage/cost rows must survive restarts (DB-only before).
    UsageRecord,
}

impl JournalType {
    /// The `type` string used in the journal line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceState => "workspace.state",
            Self::AgentState => "agent.state",
            Self::InboxEnqueue => "inbox.enqueue",
            Self::InboxDeliver => "inbox.deliver",
            Self::WorkflowTransition => "workflow.transition",
            Self::WorkflowStart => "workflow.start",
            Self::DecisionOutcome => "decision.outcome",
            Self::DecisionRecord => "decision.record",
            Self::RuleMerge => "rule.merge",
            Self::PortAlloc => "port.alloc",
            Self::PortFree => "port.free",
            Self::ProposalRecord => "proposal.record",
            Self::IntakeRecord => "intake.record",
            Self::UsageRecord => "usage.record",
        }
    }

    /// Parse a `type` string back to a variant.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "workspace.state" => Some(Self::WorkspaceState),
            "agent.state" => Some(Self::AgentState),
            "inbox.enqueue" => Some(Self::InboxEnqueue),
            "inbox.deliver" => Some(Self::InboxDeliver),
            "workflow.transition" => Some(Self::WorkflowTransition),
            "workflow.start" => Some(Self::WorkflowStart),
            "decision.outcome" => Some(Self::DecisionOutcome),
            "decision.record" => Some(Self::DecisionRecord),
            "rule.merge" => Some(Self::RuleMerge),
            "port.alloc" => Some(Self::PortAlloc),
            "port.free" => Some(Self::PortFree),
            "proposal.record" => Some(Self::ProposalRecord),
            "intake.record" => Some(Self::IntakeRecord),
            "usage.record" => Some(Self::UsageRecord),
            _ => None,
        }
    }
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalRecord {
    pub seq: u64,
    pub r#type: JournalType,
    /// The full new-state payload (idempotent).
    pub data: serde_json::Value,
    pub ts: String,
}

impl JournalRecord {
    /// Serialize to a single JSONL line.
    #[must_use]
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse one JSONL line.
    ///
    /// # Errors
    /// [`CoreError::MalformedRecord`] when the line is not a well-formed
    /// journal record.
    pub fn from_line(line: &str) -> Result<Self, CoreError> {
        serde_json::from_str::<Self>(line)
            .map_err(|e| CoreError::MalformedRecord(format!("invalid journal line: {e}")))
    }

    /// Parse the `data` payload as a typed event.
    #[must_use]
    pub fn as_workspace(&self) -> Option<Workspace> {
        serde_json::from_value(self.data.clone()).ok()
    }

    #[must_use]
    pub fn as_agent_state(&self) -> Option<AgentStateEvent> {
        serde_json::from_value(self.data.clone()).ok()
    }

    #[must_use]
    pub fn as_inbox(&self) -> Option<InboxEntry> {
        serde_json::from_value(self.data.clone()).ok()
    }

    #[must_use]
    pub fn as_inbox_deliver(&self) -> Option<InboxDeliverEvent> {
        serde_json::from_value(self.data.clone()).ok()
    }

    #[must_use]
    pub fn as_workflow_transition(&self) -> Option<WorkflowTransitionEvent> {
        serde_json::from_value(self.data.clone()).ok()
    }

    #[must_use]
    pub fn as_rule(&self) -> Option<StoredRule> {
        serde_json::from_value(self.data.clone()).ok()
    }

    #[must_use]
    pub fn as_port(&self) -> Option<PortRow> {
        serde_json::from_value(self.data.clone()).ok()
    }

    /// M3: the `workflow.start` payload.
    #[must_use]
    pub fn as_workflow_start(&self) -> Option<WorkflowStartEvent> {
        serde_json::from_value(self.data.clone()).ok()
    }
}

/// M3: payload of `workflow.start` — a workflow was started for `(ws, graph)`
/// with render variables, so a restart can restore the instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStartEvent {
    pub ws: String,
    pub graph: String,
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, String>,
}

/// Payload of `agent.state`: the full new agent row, so replay rebuilds the
/// `agent` projection without extra lookups (journal records are idempotent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStateEvent {
    pub workspace_id: String,
    pub agent_id: String,
    pub role: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub driver: crate::types::DriverKind,
    #[serde(default)]
    pub mode: crate::types::AgentMode,
    pub state: AgentState,
    pub confidence: f64,
}

impl From<crate::types::Agent> for AgentStateEvent {
    fn from(a: crate::types::Agent) -> Self {
        Self {
            workspace_id: a.workspace_id,
            agent_id: a.agent_id,
            role: a.role,
            model: a.model,
            session_id: a.session_id,
            driver: a.driver,
            mode: a.mode,
            state: a.state,
            confidence: a.confidence,
        }
    }
}

impl From<AgentStateEvent> for crate::types::Agent {
    fn from(e: AgentStateEvent) -> Self {
        Self {
            workspace_id: e.workspace_id,
            agent_id: e.agent_id,
            role: e.role,
            model: e.model,
            session_id: e.session_id,
            driver: e.driver,
            mode: e.mode,
            state: e.state,
            confidence: e.confidence,
        }
    }
}

/// Payload of `inbox.deliver`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxDeliverEvent {
    pub id: String,
    pub delivered_at: String,
}

/// Payload of `workflow.transition` (a `node_state` row mutation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTransitionEvent {
    pub graph_id: String,
    pub node_id: String,
    pub state: NodeState,
    pub attempt: u32,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

impl From<WorkflowTransitionEvent> for crate::types::NodeStateRow {
    fn from(e: WorkflowTransitionEvent) -> Self {
        Self {
            graph_id: e.graph_id,
            node_id: e.node_id,
            state: e.state,
            attempt: e.attempt,
            started_at: e.started_at,
            finished_at: e.finished_at,
            error: e.error,
        }
    }
}

impl From<crate::types::NodeStateRow> for WorkflowTransitionEvent {
    fn from(r: crate::types::NodeStateRow) -> Self {
        Self {
            graph_id: r.graph_id,
            node_id: r.node_id,
            state: r.state,
            attempt: r.attempt,
            started_at: r.started_at,
            finished_at: r.finished_at,
            error: r.error,
        }
    }
}

/// The result of replaying a journal file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JournalReplay {
    pub records: Vec<JournalRecord>,
    /// Corrupt lines, `(line_number, reason)`.
    pub skipped: Vec<(usize, String)>,
}

/// Replay a journal: parse every line, keeping well-formed records and
/// accounting for corrupt lines (skipped with a warning + count, per §10).
#[must_use]
pub fn replay(input: &str) -> JournalReplay {
    let mut replay = JournalReplay::default();
    for (idx, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match JournalRecord::from_line(line) {
            Ok(record) => replay.records.push(record),
            Err(e) => replay.skipped.push((idx + 1, e.to_string())),
        }
    }
    replay
}

/// Replay records but collapse duplicates by `seq` (a replayed tail appended
/// after a crash may repeat a line; records are idempotent so one copy wins).
#[must_use]
pub fn replay_dedup(input: &str) -> JournalReplay {
    let mut replay = replay(input);
    let mut seen = std::collections::BTreeSet::new();
    replay.records.retain(|r| seen.insert(r.seq));
    replay
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> Workspace {
        Workspace {
            id: "iot".to_owned(),
            path: "/Users/u/iot".to_owned(),
            port: Some(4101),
            server_pid: Some(4242),
            state: crate::types::WorkspaceState::On,
            cmux_ws: Some("ws_1".to_owned()),
            layout_path: None,
            updated_at: "2026-08-13T00:00:00.000Z".to_owned(),
        }
    }

    #[test]
    fn record_roundtrips_through_a_line() {
        let record = JournalRecord {
            seq: 3,
            r#type: JournalType::WorkspaceState,
            data: serde_json::to_value(workspace()).unwrap(),
            ts: "2026-08-13T00:00:00.000Z".to_owned(),
        };
        let line = record.to_line();
        let back = JournalRecord::from_line(&line).unwrap();
        assert_eq!(back, record);
        assert_eq!(back.as_workspace(), Some(workspace()));
    }

    #[test]
    fn malformed_line_is_rejected() {
        assert!(JournalRecord::from_line("not json").is_err());
        assert!(JournalRecord::from_line(r#"{"seq":1}"#).is_err(), "missing type/data/ts");
    }

    #[test]
    fn journal_type_strings_roundtrip() {
        for t in [
            JournalType::WorkspaceState,
            JournalType::AgentState,
            JournalType::InboxEnqueue,
            JournalType::InboxDeliver,
            JournalType::WorkflowTransition,
            JournalType::WorkflowStart,
            JournalType::DecisionOutcome,
            JournalType::DecisionRecord,
            JournalType::RuleMerge,
            JournalType::PortAlloc,
            JournalType::PortFree,
        ] {
            assert_eq!(JournalType::parse(t.as_str()), Some(t));
        }
        assert_eq!(JournalType::parse("bogus"), None);
    }

    #[test]
    fn replay_skips_corrupt_lines_with_a_count() {
        let good = JournalRecord {
            seq: 1,
            r#type: JournalType::PortAlloc,
            data: serde_json::to_value(PortRow {
                port: 4101,
                workspace_id: "iot".to_owned(),
                allocated_at: "2026-08-13T00:00:00.000Z".to_owned(),
            })
            .unwrap(),
            ts: "2026-08-13T00:00:00.000Z".to_owned(),
        };
        let input = format!("{}\ncorrupt-tail-here\n", good.to_line());
        let replay = replay(&input);
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.skipped.len(), 1);
        assert_eq!(replay.skipped[0].0, 2, "the second line was corrupt");
    }

    #[test]
    fn replay_dedup_collapses_repeated_seqs() {
        let a = JournalRecord {
            seq: 1,
            r#type: JournalType::PortAlloc,
            data: serde_json::json!({ "port": 4101, "workspace_id": "iot", "allocated_at": "t" }),
            ts: "t".to_owned(),
        };
        let b = JournalRecord {
            seq: 2,
            r#type: JournalType::AgentState,
            data: serde_json::json!({ "workspace_id": "iot", "agent_id": "dev_01", "state": "idle", "confidence": 1.0 }),
            ts: "t".to_owned(),
        };
        let input = format!("{}\n{}\n{}\n", a.to_line(), b.to_line(), a.to_line());
        let replay = replay_dedup(&input);
        assert_eq!(replay.records.len(), 2, "duplicate seq 1 collapsed");
        assert_eq!(replay.records[0].seq, 1);
        assert_eq!(replay.records[1].seq, 2);
    }

    #[test]
    fn typed_payloads_decode() {
        let agent = JournalRecord {
            seq: 1,
            r#type: JournalType::AgentState,
            data: serde_json::json!({
                "workspace_id": "iot", "agent_id": "dev_01", "role": "dev",
                "model": "m", "session_id": "s1", "state": "working", "confidence": 1.0
            }),
            ts: "t".to_owned(),
        };
        let e = agent.as_agent_state().unwrap();
        assert_eq!(e.state, AgentState::Working);
        assert_eq!(e.session_id.as_deref(), Some("s1"));
        let a: crate::types::Agent = e.into();
        assert_eq!(a.role, "dev");
    }

    #[test]
    fn empty_lines_are_ignored() {
        let replay = replay("\n\n");
        assert!(replay.records.is_empty());
        assert!(replay.skipped.is_empty());
    }
}
