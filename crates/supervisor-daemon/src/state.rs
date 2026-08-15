//! The in-memory fleet state (C2) with journal-first mutations (§3.2, §3.3).
//!
//! [`FleetState`] is the single in-process authority: the journal is the
//! source of truth, the in-memory maps mirror it, and the `SQLite` [`Store`] is
//! a rebuildable projection. Every mutation follows the same order: **journal
//! first, then memory, then the projection**. On open the journal is replayed
//! into memory and the projection is rebuilt from it (dropping any stale DB),
//! so there is never a second master.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use supervisor_core::journal::{JournalRecord, JournalType};
use supervisor_core::now_rfc3339;
use supervisor_core::types::{
    Agent, DecisionRecord, Graph, InboxEntry, IntakeItem, NodeState, NodeStateRow, PortRow,
    Proposal, StoredRule, Workspace, WorkspaceState,
};

use crate::db::Store;
use crate::journal::Journal;

/// The in-memory mirror of the fleet.
#[derive(Debug, Default)]
pub struct FleetState {
    workspaces: BTreeMap<String, Workspace>,
    agents: BTreeMap<(String, String), Agent>,
    ports: BTreeMap<u16, PortRow>,
    inboxes: BTreeMap<(String, String), VecDeque<InboxEntry>>,
    graphs: BTreeMap<String, Graph>,
    node_states: BTreeMap<(String, String), NodeStateRow>,
    decisions: Vec<DecisionRecord>,
    rules: BTreeMap<String, StoredRule>,
    proposals: BTreeMap<String, Proposal>,
    intake: VecDeque<IntakeItem>,
    /// M3: workflows started (`ws, graph, vars`) — journaled so a restart can
    /// restore running instances.
    workflow_starts: Vec<(String, String, std::collections::BTreeMap<String, String>)>,
}

/// A fleet state bound to its journal and projection. `journal` and `store`
/// are owned here so the append→mirror→project ordering is one call.
pub struct Fleet {
    pub state: FleetState,
    pub journal: Journal,
    pub store: Store,
    /// M10: the state dir, for the `fleet.json` projection writer.
    state_dir: PathBuf,
}

/// The `fleet.json` projection (§3.3): a human-readable snapshot of the fleet,
/// rebuilt from the in-memory state (the journal remains the source of truth).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetProjection {
    pub workspaces: Vec<Workspace>,
    pub agents: Vec<Agent>,
    pub updated_at: String,
}

impl Fleet {
    /// Open (or create) the fleet at `state_dir`: open the journal and store,
    /// replay the journal into memory, then rebuild the projection from the
    /// replay (dropping any stale DB — the journal always wins).
    ///
    /// # Errors
    /// Any I/O or `SQLite` failure while opening.
    pub fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("creating state dir {}", state_dir.display()))?;
        let journal = Journal::open(&state_dir.join("journal.jsonl"))?;
        let store = Store::open(&state_dir.join("supervisor.db"))?;
        let (records, skipped) = journal.replay_file()?;
        if !skipped.is_empty() {
            tracing::warn!(count = skipped.len(), "journal replay skipped corrupt lines");
        }
        let mut state = FleetState::default();
        store.rebuild()?;
        let mut projected = 0usize;
        for record in &records {
            state.apply(record)?;
            store.apply(record)?;
            projected += 1;
        }
        tracing::info!(records = projected, "fleet state rebuilt from journal");
        Ok(Self { state, journal, store, state_dir: state_dir.to_owned() })
    }

    /// M10: atomically rewrite the `fleet.json` projection (§3.3) from the
    /// in-memory state (tmp file + rename). The journal remains the source of
    /// truth; this is a cache for humans/dashboard/supervisor-agent.
    ///
    /// # Errors
    /// Any I/O failure.
    pub fn write_projection(&self) -> Result<()> {
        let projection = FleetProjection {
            workspaces: self.state.workspaces.values().cloned().collect(),
            agents: self.state.agents.values().cloned().collect(),
            updated_at: now_rfc3339(),
        };
        let bytes = serde_json::to_vec_pretty(&projection).context("encode fleet.json")?;
        let final_path = self.state_dir.join("fleet.json");
        let tmp_path = self.state_dir.join("fleet.json.tmp");
        std::fs::write(&tmp_path, &bytes)
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &final_path)
            .with_context(|| format!("renaming onto {}", final_path.display()))
    }

    /// The sequence number of the next journal write.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.journal.next_seq()
    }

    // --- journal-first mutations ------------------------------------------

    /// Replace a workspace. Idempotent: carries the full new value.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn upsert_workspace(&mut self, ws: &Workspace) -> Result<JournalRecord> {
        let record = self.journal.append(JournalType::WorkspaceState, serde_json::to_value(ws)?)?;
        self.state.workspaces.insert(ws.id.clone(), ws.clone());
        self.store.upsert_workspace(ws)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Replace an agent. Idempotent: carries the full new value.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn upsert_agent(&mut self, agent: &Agent) -> Result<JournalRecord> {
        let record = self.journal.append(JournalType::AgentState, serde_json::to_value(agent)?)?;
        self.state
            .agents
            .insert((agent.workspace_id.clone(), agent.agent_id.clone()), agent.clone());
        self.store.upsert_agent(agent)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Allocate a port to a workspace (recorded for adopt-or-kill).
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn alloc_port(&mut self, port: u16, workspace_id: &str) -> Result<JournalRecord> {
        let row = PortRow {
            port,
            workspace_id: workspace_id.to_owned(),
            allocated_at: supervisor_core::now_rfc3339(),
        };
        let record = self.journal.append(JournalType::PortAlloc, serde_json::to_value(&row)?)?;
        self.state.ports.insert(port, row.clone());
        self.store.upsert_port(&row)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Free a port.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn free_port(&mut self, port: u16, workspace_id: &str) -> Result<JournalRecord> {
        let record = self.journal.append(
            JournalType::PortFree,
            serde_json::json!({ "port": port, "workspace_id": workspace_id }),
        )?;
        self.state.ports.remove(&port);
        self.store.delete_port(port)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Enqueue an inbox entry.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn enqueue_inbox(&mut self, entry: &InboxEntry) -> Result<JournalRecord> {
        let record =
            self.journal.append(JournalType::InboxEnqueue, serde_json::to_value(entry)?)?;
        self.state
            .inboxes
            .entry((entry.workspace_id.clone(), entry.agent_id.clone()))
            .or_default()
            .push_back(entry.clone());
        self.store.enqueue_inbox(entry)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Mark an inbox entry delivered.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn deliver_inbox(&mut self, id: &str) -> Result<JournalRecord> {
        let delivered_at = supervisor_core::now_rfc3339();
        let record = self.journal.append(
            JournalType::InboxDeliver,
            serde_json::json!({ "id": id, "delivered_at": delivered_at }),
        )?;
        if let Some(entry) = self.state.inboxes.values_mut().flatten().find(|e| e.id == id) {
            entry.delivered = true;
            entry.delivered_at = Some(delivered_at.clone());
        }
        self.store.mark_delivered(id, &delivered_at)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Set a node state row.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn set_node_state(&mut self, row: &NodeStateRow) -> Result<JournalRecord> {
        let record =
            self.journal.append(JournalType::WorkflowTransition, serde_json::to_value(row)?)?;
        self.state.node_states.insert((row.graph_id.clone(), row.node_id.clone()), row.clone());
        self.store.set_node_state(row)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Upsert a graph.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn upsert_graph(&mut self, graph: &Graph) -> Result<JournalRecord> {
        let record =
            self.journal.append(JournalType::WorkflowTransition, serde_json::to_value(graph)?)?;
        self.state.graphs.insert(graph.id.clone(), graph.clone());
        self.store.upsert_graph(graph)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Append a decision record.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn append_decision(&mut self, d: &DecisionRecord) -> Result<JournalRecord> {
        let record = self.journal.append(JournalType::DecisionRecord, serde_json::to_value(d)?)?;
        self.state.decisions.push(d.clone());
        self.store.append_decision(d)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// M10: record a decision outcome (journal-first; bake-back's confidence
    /// reads recorded outcomes and they survive restart via the journal).
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn record_decision_outcome(&mut self, id: &str, outcome: &serde_json::Value) -> Result<()> {
        let record = self.journal.append(
            JournalType::DecisionOutcome,
            serde_json::json!({ "id": id, "outcome": outcome }),
        )?;
        if let Some(decision) = self.state.decisions.iter_mut().find(|d| d.id == id) {
            decision.outcome = Some(outcome.clone());
        }
        self.store.set_decision_outcome(id, outcome)?;
        self.store.journal_row(&record)?;
        Ok(())
    }

    /// Merge a rule into the rule table.
    ///
    /// # Errors
    /// Any journal or projection failure.
    pub fn upsert_rule(&mut self, r: &StoredRule) -> Result<JournalRecord> {
        let record = self.journal.append(JournalType::RuleMerge, serde_json::to_value(r)?)?;
        self.state.rules.insert(r.id.clone(), r.clone());
        self.store.upsert_rule(r)?;
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// Persist a proposal (stable across restarts).
    ///
    /// # Errors
    /// Any projection failure.
    pub fn upsert_proposal(&mut self, p: &Proposal) -> Result<()> {
        self.state.proposals.insert(p.id.clone(), p.clone());
        self.store.upsert_proposal(p)
    }

    /// Insert an intake item.
    ///
    /// # Errors
    /// Any projection failure.
    pub fn insert_intake(&mut self, item: &IntakeItem) -> Result<()> {
        self.state.intake.push_back(item.clone());
        self.store.insert_intake(item)
    }

    /// Insert a usage row (U5, idempotent by id).
    ///
    /// # Errors
    /// Any projection failure.
    pub fn insert_usage(&mut self, row: &supervisor_core::types::UsageRow) -> Result<()> {
        self.store.insert_usage(row)
    }

    /// Usage rows, filtered by workspace/agent and since a ts.
    ///
    /// # Errors
    /// Any projection failure.
    pub fn usage_since(
        &self,
        workspace: Option<&str>,
        agent: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<supervisor_core::types::UsageRow>> {
        self.store.usage_since(workspace, agent, since)
    }

    /// Aggregate counts for the metrics endpoint (§3.4): nodes in a terminal
    /// state, delivered messages, agents in error. Current-state snapshots.
    #[must_use]
    pub fn aggregate_counts(&self) -> (u64, u64, u64, u64) {
        let mut done = 0u64;
        let mut failed = 0u64;
        for row in self.state.node_states.values() {
            match row.state {
                NodeState::Done => done += 1,
                NodeState::Failed => failed += 1,
                _ => {}
            }
        }
        let delivered =
            self.state.inboxes.values().flatten().filter(|e| e.delivered).count() as u64;
        let errors = self
            .state
            .agents
            .values()
            .filter(|a| a.state == supervisor_core::types::AgentState::Error)
            .count() as u64;
        (done, failed, delivered, errors)
    }

    /// M3: journal a workflow start (journal-first, idempotent on replay).
    ///
    /// # Errors
    /// Any journal failure.
    pub fn record_workflow_start(
        &mut self,
        ws: &str,
        graph: &str,
        vars: &std::collections::BTreeMap<String, String>,
    ) -> Result<JournalRecord> {
        let record = self.journal.append(
            JournalType::WorkflowStart,
            serde_json::to_value(supervisor_core::journal::WorkflowStartEvent {
                ws: ws.to_owned(),
                graph: graph.to_owned(),
                vars: vars.clone(),
            })?,
        )?;
        self.state.workflow_starts.push((ws.to_owned(), graph.to_owned(), vars.clone()));
        self.store.journal_row(&record)?;
        Ok(record)
    }

    /// M3: workflows started since the journal began.
    #[must_use = "iterators are lazy; the results are only produced when consumed"]
    pub fn workflow_starts(
        &self,
    ) -> impl Iterator<Item = (&str, &str, &std::collections::BTreeMap<String, String>)> {
        self.state
            .workflow_starts
            .iter()
            .map(|(ws, graph, vars)| (ws.as_str(), graph.as_str(), vars))
    }

    /// Link an intake item to the workflow graph started for it (F3).
    ///
    /// # Errors
    /// Any projection failure.
    pub fn link_intake(&mut self, id: &str, graph_id: &str) -> Result<()> {
        if let Some(item) = self.state.intake.iter_mut().find(|i| i.id == id) {
            item.graph_id = Some(graph_id.to_owned());
        }
        self.store.link_intake(id, graph_id)
    }

    // --- read accessors ----------------------------------------------------

    #[must_use]
    pub fn workspace(&self, id: &str) -> Option<&Workspace> {
        self.state.workspaces.get(id)
    }

    #[must_use = "iterators are lazy; the results are only produced when consumed"]
    pub fn workspaces(&self) -> impl Iterator<Item = &Workspace> {
        self.state.workspaces.values()
    }

    #[must_use]
    pub fn agent(&self, ws: &str, agent: &str) -> Option<&Agent> {
        self.state.agents.get(&(ws.to_owned(), agent.to_owned()))
    }

    #[must_use = "iterators are lazy; the results are only produced when consumed"]
    pub fn agents(&self, ws: &str) -> impl Iterator<Item = &Agent> {
        self.state.agents.iter().filter(move |((wid, _), _)| wid == ws).map(|(_, a)| a)
    }

    #[must_use]
    pub fn port_of(&self, ws: &str) -> Option<u16> {
        self.state.ports.iter().find(|(_, p)| p.workspace_id == ws).map(|(port, _)| *port)
    }

    #[must_use]
    pub fn workspace_for_port(&self, port: u16) -> Option<&str> {
        self.state.ports.get(&port).map(|p| p.workspace_id.as_str())
    }

    #[must_use]
    pub fn undelivered(&self, ws: &str, agent: &str) -> Vec<&InboxEntry> {
        self.state
            .inboxes
            .get(&(ws.to_owned(), agent.to_owned()))
            .map(|q| q.iter().filter(|e| !e.delivered).collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn inbox_depth(&self, ws: &str, agent: &str) -> usize {
        self.undelivered(ws, agent).len()
    }

    #[must_use]
    pub fn graph(&self, id: &str) -> Option<&Graph> {
        self.state.graphs.get(id)
    }

    /// Every stored graph, in id order.
    #[must_use = "iterators are lazy; the results are only produced when consumed"]
    pub fn graphs(&self) -> impl Iterator<Item = &Graph> {
        self.state.graphs.values()
    }

    #[must_use]
    pub fn node_state(&self, graph: &str, node: &str) -> Option<&NodeStateRow> {
        self.state.node_states.get(&(graph.to_owned(), node.to_owned()))
    }

    /// Node-state rows for a graph.
    #[must_use = "iterators are lazy; the results are only produced when consumed"]
    pub fn node_states(&self, graph: &str) -> impl Iterator<Item = &NodeStateRow> {
        self.state.node_states.iter().filter(move |((g, _), _)| g == graph).map(|(_, row)| row)
    }

    #[must_use]
    pub fn decisions(&self) -> &[DecisionRecord] {
        &self.state.decisions
    }

    #[must_use = "iterators are lazy; the results are only produced when consumed"]
    pub fn rules(&self) -> impl Iterator<Item = &StoredRule> {
        self.state.rules.values()
    }

    #[must_use]
    pub fn proposal(&self, id: &str) -> Option<&Proposal> {
        self.state.proposals.get(id)
    }

    #[must_use = "iterators are lazy; the results are only produced when consumed"]
    pub fn proposals(&self) -> impl Iterator<Item = &Proposal> {
        self.state.proposals.values()
    }

    #[must_use = "iterators are lazy; the results are only produced when consumed"]
    pub fn intake(&self) -> impl Iterator<Item = &IntakeItem> {
        self.state.intake.iter()
    }

    /// One intake item by id.
    #[must_use]
    pub fn intake_item(&self, id: &str) -> Option<&IntakeItem> {
        self.state.intake.iter().find(|i| i.id == id)
    }

    /// Workspaces flagged to resume on start (not `off`).
    #[must_use]
    pub fn resume_list(&self) -> Vec<&Workspace> {
        self.state.workspaces.values().filter(|w| w.state != WorkspaceState::Off).collect()
    }
}

impl FleetState {
    /// Apply one journal record to the in-memory mirror. Records carry the
    /// full new state value, so replay is idempotent.
    ///
    /// # Errors
    /// A malformed-but-parseable record whose payload does not decode returns
    /// an error; the caller decides whether to skip or halt.
    fn apply(&mut self, record: &JournalRecord) -> Result<()> {
        match record.r#type {
            JournalType::WorkspaceState => {
                let ws =
                    record.as_workspace().context("workspace.state payload does not decode")?;
                self.workspaces.insert(ws.id.clone(), ws);
            }
            JournalType::AgentState => {
                let event =
                    record.as_agent_state().context("agent.state payload does not decode")?;
                let agent: Agent = event.into();
                self.agents.insert((agent.workspace_id.clone(), agent.agent_id.clone()), agent);
            }
            JournalType::InboxEnqueue => {
                let entry = record.as_inbox().context("inbox.enqueue payload does not decode")?;
                self.inboxes
                    .entry((entry.workspace_id.clone(), entry.agent_id.clone()))
                    .or_default()
                    .push_back(entry);
            }
            JournalType::InboxDeliver => {
                let event =
                    record.as_inbox_deliver().context("inbox.deliver payload does not decode")?;
                if let Some(entry) = self.inboxes.values_mut().flatten().find(|e| e.id == event.id)
                {
                    entry.delivered = true;
                    entry.delivered_at = Some(event.delivered_at);
                }
            }
            JournalType::WorkflowTransition => {
                // The payload is either a NodeStateRow or a Graph; detect by
                // the presence of a `graph_id` field.
                if let Some(event) = record.as_workflow_transition() {
                    let row: NodeStateRow = event.into();
                    self.node_states.insert((row.graph_id.clone(), row.node_id.clone()), row);
                } else if let Ok(graph) = serde_json::from_value::<Graph>(record.data.clone()) {
                    self.graphs.insert(graph.id.clone(), graph);
                }
            }
            JournalType::DecisionRecord => {
                if let Ok(d) = serde_json::from_value::<DecisionRecord>(record.data.clone()) {
                    self.decisions.push(d);
                }
            }
            JournalType::RuleMerge => {
                if let Some(r) = record.as_rule() {
                    self.rules.insert(r.id.clone(), r);
                }
            }
            JournalType::PortAlloc => {
                if let Some(row) = record.as_port() {
                    self.ports.insert(row.port, row);
                }
            }
            JournalType::PortFree => {
                if let Some(port) = record.data.get("port").and_then(serde_json::Value::as_u64) {
                    self.ports.remove(&u16::try_from(port).unwrap_or(u16::MAX));
                }
            }
            JournalType::WorkflowStart => {
                if let Some(event) = record.as_workflow_start() {
                    self.workflow_starts.push((event.ws, event.graph, event.vars));
                }
            }
            JournalType::DecisionOutcome => {
                if let Some(id) = record.data.get("id").and_then(serde_json::Value::as_str)
                    && let Some(outcome) = record.data.get("outcome")
                    && let Some(decision) = self.decisions.iter_mut().find(|d| d.id == id)
                {
                    decision.outcome = Some(outcome.clone());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use supervisor_core::types::{AgentState, Priority};

    fn fleet() -> (Fleet, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let fleet = Fleet::open(dir.path()).unwrap();
        (fleet, dir)
    }

    fn ws(id: &str) -> Workspace {
        Workspace {
            id: id.to_owned(),
            path: format!("/x/{id}"),
            port: Some(4101),
            server_pid: Some(1234),
            state: WorkspaceState::On,
            cmux_ws: Some("w".to_owned()),
            layout_path: None,
            updated_at: "t".to_owned(),
        }
    }

    fn agent() -> Agent {
        Agent {
            workspace_id: "iot".to_owned(),
            agent_id: "dev_01".to_owned(),
            role: "dev".to_owned(),
            model: Some("m".to_owned()),
            session_id: Some("s1".to_owned()),
            driver: supervisor_core::types::DriverKind::Opencode,
            mode: supervisor_core::types::AgentMode::Foreground,
            state: AgentState::Idle,
            confidence: 1.0,
        }
    }

    #[test]
    fn mutations_persist_across_reopen() {
        let (mut fleet, dir) = fleet();
        fleet.upsert_workspace(&ws("iot")).unwrap();
        fleet.upsert_agent(&agent()).unwrap();
        fleet.alloc_port(4101, "iot").unwrap();
        drop(fleet);

        let reopened = Fleet::open(dir.path()).unwrap();
        assert_eq!(reopened.workspace("iot").unwrap().state, WorkspaceState::On);
        assert_eq!(reopened.agent("iot", "dev_01").unwrap().session_id.as_deref(), Some("s1"));
        assert_eq!(reopened.port_of("iot"), Some(4101));
    }

    #[test]
    fn inbox_delivery_survives_restart() {
        let (mut fleet, dir) = fleet();
        fleet.upsert_workspace(&ws("iot")).unwrap();
        let entry = InboxEntry {
            id: "e1".to_owned(),
            workspace_id: "iot".to_owned(),
            agent_id: "dev_01".to_owned(),
            priority: Priority::High,
            body: "do it".to_owned(),
            from: "human".to_owned(),
            kind: "instruction".to_owned(),
            in_reply_to: None,
            ack_for: None,
            delivered: false,
            delivered_at: None,
            created_at: "t".to_owned(),
        };
        fleet.enqueue_inbox(&entry).unwrap();
        assert_eq!(fleet.inbox_depth("iot", "dev_01"), 1);
        fleet.deliver_inbox("e1").unwrap();
        assert_eq!(fleet.inbox_depth("iot", "dev_01"), 0);
        drop(fleet);

        let reopened = Fleet::open(dir.path()).unwrap();
        assert_eq!(reopened.inbox_depth("iot", "dev_01"), 0, "delivered stays delivered");
        assert_eq!(reopened.undelivered("iot", "dev_01").len(), 0);
    }

    #[test]
    fn node_state_and_graph_replay() {
        let (mut fleet, dir) = fleet();
        let graph = Graph {
            id: "bug_flow".to_owned(),
            name: "n".to_owned(),
            data: "{}".to_owned(),
            version: 1,
            active: true,
            updated_at: "t".to_owned(),
        };
        fleet.upsert_graph(&graph).unwrap();
        let row = NodeStateRow {
            graph_id: "bug_flow".to_owned(),
            node_id: "fix".to_owned(),
            state: supervisor_core::types::NodeState::Running,
            attempt: 1,
            started_at: Some("t".to_owned()),
            finished_at: None,
            error: None,
        };
        fleet.set_node_state(&row).unwrap();
        drop(fleet);

        let reopened = Fleet::open(dir.path()).unwrap();
        assert!(reopened.graph("bug_flow").is_some());
        assert_eq!(
            reopened.node_state("bug_flow", "fix").unwrap().state,
            supervisor_core::types::NodeState::Running
        );
    }

    #[test]
    fn stale_projection_is_rebuilt_from_journal() {
        let (mut fleet, dir) = fleet();
        fleet.upsert_workspace(&ws("iot")).unwrap();
        drop(fleet);
        // Corrupt the projection by deleting the DB file; reopen must rebuild
        // from the journal.
        std::fs::remove_file(dir.path().join("supervisor.db")).unwrap();
        let reopened = Fleet::open(dir.path()).unwrap();
        assert_eq!(reopened.workspace("iot").unwrap().state, WorkspaceState::On);
    }

    #[test]
    fn write_projection_emits_fleet_json() {
        let (mut fleet, dir) = fleet();
        fleet.upsert_workspace(&ws("iot")).unwrap();
        fleet.write_projection().unwrap();
        let path = dir.path().join("fleet.json");
        assert!(path.exists());
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["workspaces"][0]["id"], "iot");
        assert!(!path.join("fleet.json.tmp").exists(), "no leftover tmp file");
    }

    #[test]
    fn decision_outcome_is_recorded() {
        let (mut fleet, dir) = fleet();
        fleet.upsert_workspace(&ws("iot")).unwrap();
        let record = supervisor_core::types::DecisionRecord {
            id: "dec_1".to_owned(),
            signature: "sig".to_owned(),
            situation: serde_json::json!({}),
            decision: serde_json::json!({"kind": "post"}),
            outcome: None,
            ts: "t".to_owned(),
        };
        fleet.append_decision(&record).unwrap();
        fleet
            .record_decision_outcome(
                "dec_1",
                &serde_json::json!({"result": "applied", "success": true}),
            )
            .unwrap();
        let outcome = fleet
            .decisions()
            .iter()
            .find(|d| d.id == "dec_1")
            .unwrap()
            .outcome
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(outcome["success"], true);
        // Survives a reopen (projection persisted).
        drop(fleet);
        let reopened = Fleet::open(dir.path()).unwrap();
        assert!(
            reopened.decisions().iter().any(|d| d.id == "dec_1" && d.outcome.as_ref().is_some())
        );
    }

    #[test]
    fn resume_list_excludes_off_workspaces() {
        let (mut fleet, _dir) = fleet();
        let mut on = ws("on_ws");
        on.state = WorkspaceState::On;
        fleet.upsert_workspace(&on).unwrap();
        let mut off = ws("off_ws");
        off.state = WorkspaceState::Off;
        fleet.upsert_workspace(&off).unwrap();
        let resume: Vec<&str> = fleet.resume_list().iter().map(|w| w.id.as_str()).collect();
        assert_eq!(resume, vec!["on_ws"]);
    }
}
