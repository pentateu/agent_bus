//! The `SQLite` projection of fleet state (§3.1).
//!
//! The journal is the source of truth; this database is a **rebuildable
//! projection** (index/query layer). Every mutation here happens *after* the
//! matching journal entry is appended, and the DB is dropped and rebuilt from
//! the journal if the two ever disagree (§10). A single owned `Connection`
//! behind a mutex keeps footprint minimal.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use supervisor_core::types::{
    Agent, DecisionRecord, Graph, InboxEntry, IntakeItem, NodeState, NodeStateRow, PortRow,
    Proposal, ProposalStatus, StoredRule, Workspace, WorkspaceState,
};

/// The schema, verbatim from §3.1 (WAL, journal-aware).
const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS workspace (
  id          TEXT PRIMARY KEY,
  path        TEXT NOT NULL,
  port        INTEGER,
  server_pid  INTEGER,
  state       TEXT NOT NULL DEFAULT 'off',
  cmux_ws     TEXT,
  layout_path TEXT,
  updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS agent (
  workspace_id TEXT NOT NULL REFERENCES workspace(id) ON DELETE CASCADE,
  agent_id     TEXT NOT NULL,
  role         TEXT NOT NULL,
  model        TEXT,
  session_id   TEXT,
  driver       TEXT NOT NULL DEFAULT 'opencode',
  mode         TEXT NOT NULL DEFAULT 'foreground',
  state        TEXT NOT NULL DEFAULT 'unknown',
  confidence   REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY (workspace_id, agent_id)
);

CREATE TABLE IF NOT EXISTS port (
  port         INTEGER PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspace(id),
  allocated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS inbox_entry (
  id           TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  agent_id     TEXT NOT NULL,
  priority     TEXT NOT NULL DEFAULT 'normal',
  body         TEXT NOT NULL,
  "from"       TEXT,
  kind         TEXT NOT NULL DEFAULT 'instruction',
  in_reply_to  TEXT,
  ack_for      TEXT,
  delivered    INTEGER NOT NULL DEFAULT 0,
  delivered_at TEXT,
  created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS inbox_undelivered
  ON inbox_entry(workspace_id, agent_id, delivered, priority, created_at);

CREATE TABLE IF NOT EXISTS graph (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  data         TEXT NOT NULL,
  version      INTEGER NOT NULL DEFAULT 1,
  active       INTEGER NOT NULL DEFAULT 1,
  updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS node_state (
  workspace_id TEXT NOT NULL,
  graph_id     TEXT NOT NULL REFERENCES graph(id),
  node_id      TEXT NOT NULL,
  state        TEXT NOT NULL DEFAULT 'pending',
  attempt      INTEGER NOT NULL DEFAULT 0,
  started_at   TEXT,
  finished_at  TEXT,
  error        TEXT,
  PRIMARY KEY (workspace_id, graph_id, node_id)
);

CREATE TABLE IF NOT EXISTS decision (
  id          TEXT PRIMARY KEY,
  signature   TEXT NOT NULL,
  situation   TEXT NOT NULL,
  decision    TEXT NOT NULL,
  outcome     TEXT,
  ts          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS decision_sig ON decision(signature);

CREATE TABLE IF NOT EXISTS rule (
  id         TEXT PRIMARY KEY,
  toml       TEXT NOT NULL,
  source     TEXT NOT NULL,
  confidence REAL NOT NULL,
  approved   INTEGER NOT NULL DEFAULT 0,
  active     INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS proposal (
  id           TEXT PRIMARY KEY,
  rule_toml    TEXT NOT NULL,
  signature    TEXT NOT NULL,
  cluster_size INTEGER NOT NULL,
  confidence   REAL NOT NULL,
  status       TEXT NOT NULL DEFAULT 'pending',
  created_at   TEXT NOT NULL,
  resolved_at  TEXT
);

CREATE TABLE IF NOT EXISTS intake (
  id          TEXT PRIMARY KEY,
  source      TEXT NOT NULL,
  kind        TEXT NOT NULL,
  title       TEXT NOT NULL,
  body        TEXT NOT NULL,
  severity    TEXT,
  refs        TEXT NOT NULL DEFAULT '[]',
  graph_id    TEXT,
  received_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS journal (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  type       TEXT NOT NULL,
  data       TEXT NOT NULL,
  ts         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS usage (
  id               TEXT PRIMARY KEY,
  workspace_id     TEXT NOT NULL,
  agent_id         TEXT NOT NULL,
  model            TEXT,
  ts               TEXT NOT NULL,
  prompt_tokens    INTEGER NOT NULL,
  completion_tokens INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_agent ON usage(workspace_id, agent_id, ts);
"#;

/// The `SQLite` projection, one connection behind a mutex.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Apply one journal record to the projection. Called during replay so
    /// the DB is always derived from the journal.
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn apply(&self, record: &supervisor_core::journal::JournalRecord) -> Result<()> {
        use supervisor_core::journal::JournalType;
        match record.r#type {
            JournalType::WorkspaceState => {
                if let Some(ws) = record.as_workspace() {
                    self.upsert_workspace(&ws)?;
                }
            }
            JournalType::AgentState => {
                if let Some(event) = record.as_agent_state() {
                    self.upsert_agent(&event.into())?;
                }
            }
            JournalType::InboxEnqueue => {
                if let Some(entry) = record.as_inbox() {
                    self.enqueue_inbox(&entry)?;
                }
            }
            JournalType::InboxDeliver => {
                if let Some(event) = record.as_inbox_deliver() {
                    self.mark_delivered(&event.id, &event.delivered_at)?;
                }
            }
            JournalType::WorkflowTransition => {
                if let Some(event) = record.as_workflow_transition() {
                    self.set_node_state(&event.into())?;
                } else if let Ok(graph) = serde_json::from_value::<Graph>(record.data.clone()) {
                    self.upsert_graph(&graph)?;
                }
            }
            JournalType::DecisionRecord => {
                if let Ok(d) = serde_json::from_value::<DecisionRecord>(record.data.clone()) {
                    self.append_decision(&d)?;
                }
            }
            JournalType::RuleMerge => {
                if let Some(r) = record.as_rule() {
                    self.upsert_rule(&r)?;
                }
            }
            JournalType::PortAlloc => {
                if let Some(row) = record.as_port() {
                    self.upsert_port(&row)?;
                }
            }
            JournalType::PortFree => {
                if let Some(port) = record.data.get("port").and_then(serde_json::Value::as_u64) {
                    self.delete_port(u16::try_from(port).unwrap_or(u16::MAX))?;
                }
            }
            // M3: workflow starts have no projection table; the record is
            // mirrored into the `journal` table below.
            JournalType::WorkflowStart => {}
            JournalType::DecisionOutcome => {
                if let Some(id) = record.data.get("id").and_then(serde_json::Value::as_str)
                    && let Some(outcome) = record.data.get("outcome")
                {
                    self.set_decision_outcome(id, outcome)?;
                }
            }
            JournalType::ProposalRecord => {
                if let Ok(p) =
                    serde_json::from_value::<supervisor_core::types::Proposal>(record.data.clone())
                {
                    self.upsert_proposal(&p)?;
                }
            }
            JournalType::IntakeRecord => {
                if let Ok(item) = serde_json::from_value::<IntakeItem>(record.data.clone()) {
                    self.insert_intake(&item)?;
                }
            }
            JournalType::UsageRecord => {
                if let Ok(row) =
                    serde_json::from_value::<supervisor_core::types::UsageRow>(record.data.clone())
                {
                    self.insert_usage(&row)?;
                }
            }
        }
        self.journal_row(record)
    }

    /// Mirror a journal record into the `journal` table (the DB-side copy of
    /// the append-only history).
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn journal_row(&self, record: &supervisor_core::journal::JournalRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "INSERT OR IGNORE INTO journal (seq, type, data, ts) VALUES (?1, ?2, ?3, ?4)",
            params![
                i64::try_from(record.seq).unwrap_or(i64::MAX),
                record.r#type.as_str(),
                serde_json::to_string(&record.data).unwrap_or_default(),
                record.ts,
            ],
        )
        .context("mirror journal row")?;
        Ok(())
    }

    /// Open (creating + migrating) the database at `path`.
    ///
    /// # Errors
    /// Any `SQLite` open or migration failure.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating store dir {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening store {}", path.display()))?;
        // I-32: the DB mirrors journal contents; 0600 (the file may predate
        // this fix, so force it every open).
        if let Ok(metadata) = std::fs::metadata(path) {
            use std::os::unix::fs::PermissionsExt as _;
            metadata.permissions().set_mode(0o600);
        }
        conn.execute_batch(SCHEMA).context("migrating store schema")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Drop every table so the projection can be rebuilt from the journal.
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn rebuild(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute_batch(
            r"
            PRAGMA foreign_keys = OFF;
            DROP TABLE IF EXISTS journal;
            DROP TABLE IF EXISTS intake;
            DROP TABLE IF EXISTS proposal;
            DROP TABLE IF EXISTS rule;
            DROP TABLE IF EXISTS decision;
            DROP TABLE IF EXISTS node_state;
            DROP TABLE IF EXISTS graph;
            DROP TABLE IF EXISTS inbox_entry;
            DROP TABLE IF EXISTS port;
            DROP TABLE IF EXISTS agent;
            DROP TABLE IF EXISTS workspace;
            PRAGMA foreign_keys = ON;
            ",
        )
        .context("dropping projection tables")?;
        conn.execute_batch(SCHEMA).context("recreating projection schema")
    }

    /// A fresh in-memory store for tests.
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory store")?;
        conn.execute_batch(SCHEMA).context("migrating in-memory schema")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    // --- workspaces -------------------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn upsert_workspace(&self, ws: &Workspace) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            r"INSERT INTO workspace (id, path, port, state, cmux_ws, layout_path, server_pid, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT(id) DO UPDATE SET
                 path = excluded.path, port = excluded.port, state = excluded.state,
                 cmux_ws = excluded.cmux_ws, layout_path = excluded.layout_path,
                 server_pid = excluded.server_pid, updated_at = excluded.updated_at",
            params![
                ws.id,
                ws.path,
                ws.port,
                ws.state.to_db(),
                ws.cmux_ws,
                ws.layout_path,
                ws.server_pid,
                ws.updated_at,
            ],
        )
        .context("upsert workspace")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn get_workspace(&self, id: &str) -> Result<Option<Workspace>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let row = conn
            .query_row(
                "SELECT id, path, port, server_pid, state, cmux_ws, layout_path, updated_at
                 FROM workspace WHERE id = ?1",
                params![id],
                |r| Ok(workspace_from_row(r)),
            )
            .optional()
            .context("query workspace")?;
        Ok(row)
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, path, port, server_pid, state, cmux_ws, layout_path, updated_at
                 FROM workspace ORDER BY id",
            )
            .context("prepare list workspaces")?;
        let rows =
            stmt.query_map([], |r| Ok(workspace_from_row(r))).context("query list workspaces")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect workspaces")
    }

    // --- agents -----------------------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn upsert_agent(&self, a: &Agent) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "INSERT INTO agent (workspace_id, agent_id, role, model, session_id, driver, mode, state, confidence)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
               ON CONFLICT(workspace_id, agent_id) DO UPDATE SET
                 role = excluded.role, model = excluded.model, session_id = excluded.session_id,
                 driver = excluded.driver, mode = excluded.mode,
                 state = excluded.state, confidence = excluded.confidence",
            params![
                a.workspace_id,
                a.agent_id,
                a.role,
                a.model,
                a.session_id,
                a.driver.to_db(),
                a.mode.to_db(),
                a.state.to_db(),
                a.confidence
            ],
        )
        .context("upsert agent")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn list_agents(&self, workspace_id: &str) -> Result<Vec<Agent>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT workspace_id, agent_id, role, model, session_id, driver, mode, state, confidence
                 FROM agent WHERE workspace_id = ?1 ORDER BY agent_id",
            )
            .context("prepare list agents")?;
        let rows = stmt
            .query_map(params![workspace_id], |r| Ok(agent_from_row(r)))
            .context("query list agents")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect agents")
    }

    // --- ports ------------------------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn upsert_port(&self, p: &PortRow) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "INSERT OR REPLACE INTO port (port, workspace_id, allocated_at) VALUES (?1, ?2, ?3)",
            params![p.port, p.workspace_id, p.allocated_at],
        )
        .context("upsert port")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn delete_port(&self, port: u16) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute("DELETE FROM port WHERE port = ?1", params![port]).context("delete port")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn list_ports(&self) -> Result<Vec<PortRow>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare("SELECT port, workspace_id, allocated_at FROM port ORDER BY port")
            .context("prepare list ports")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PortRow { port: r.get(0)?, workspace_id: r.get(1)?, allocated_at: r.get(2)? })
            })
            .context("query list ports")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect ports")
    }

    // --- inbox ------------------------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn enqueue_inbox(&self, e: &InboxEntry) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            r#"INSERT INTO inbox_entry
                 (id, workspace_id, agent_id, priority, body, "from", kind, in_reply_to, ack_for, delivered, delivered_at, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"#,
            params![
                e.id,
                e.workspace_id,
                e.agent_id,
                e.priority.to_db(),
                e.body,
                e.from,
                e.kind,
                e.in_reply_to,
                e.ack_for,
                e.delivered,
                e.delivered_at,
                e.created_at,
            ],
        )
        .context("enqueue inbox")?;
        Ok(())
    }

    /// Claim the next undelivered entries for an agent, ordered by
    /// `(priority desc, created_at)`.
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn claim_inbox(
        &self,
        workspace_id: &str,
        agent_id: &str,
        limit: usize,
    ) -> Result<Vec<InboxEntry>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                r#"SELECT id, workspace_id, agent_id, priority, body, "from", kind, in_reply_to, ack_for, delivered, delivered_at, created_at
                   FROM inbox_entry
                   WHERE workspace_id = ?1 AND agent_id = ?2 AND delivered = 0
                   ORDER BY CASE priority WHEN 'high' THEN 0 ELSE 1 END, created_at
                   LIMIT ?3"#,
            )
            .context("prepare claim inbox")?;
        let rows = stmt
            .query_map(
                params![workspace_id, agent_id, i64::try_from(limit).unwrap_or(i64::MAX)],
                |r| Ok(inbox_from_row(r)),
            )
            .context("query claim inbox")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect inbox")
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn mark_delivered(&self, id: &str, delivered_at: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "UPDATE inbox_entry SET delivered = 1, delivered_at = ?2 WHERE id = ?1",
            params![id, delivered_at],
        )
        .context("mark inbox delivered")?;
        Ok(())
    }

    /// Undelivered entry count for an agent.
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn inbox_depth(&self, workspace_id: &str, agent_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = conn
            .query_row(
                "SELECT COUNT(*) FROM inbox_entry
                 WHERE workspace_id = ?1 AND agent_id = ?2 AND delivered = 0",
                params![workspace_id, agent_id],
                |r| r.get::<_, i64>(0),
            )
            .context("query inbox depth")?;
        Ok(usize::try_from(n).unwrap_or(usize::MAX))
    }

    // --- graphs + node state ---------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn upsert_graph(&self, g: &Graph) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            r"INSERT INTO graph (id, name, data, version, active, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)
               ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name, data = excluded.data, version = excluded.version,
                 active = excluded.active, updated_at = excluded.updated_at",
            params![g.id, g.name, g.data, g.version, g.active, g.updated_at],
        )
        .context("upsert graph")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn get_graph(&self, id: &str) -> Result<Option<Graph>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let row = conn
            .query_row(
                "SELECT id, name, data, version, active, updated_at FROM graph WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Graph {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        data: r.get(2)?,
                        version: r.get(3)?,
                        active: r.get::<_, i64>(4)? != 0,
                        updated_at: r.get(5)?,
                    })
                },
            )
            .optional()
            .context("query graph")?;
        Ok(row)
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn list_graphs(&self) -> Result<Vec<Graph>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare("SELECT id, name, data, version, active, updated_at FROM graph ORDER BY id")
            .context("prepare list graphs")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Graph {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    data: r.get(2)?,
                    version: r.get(3)?,
                    active: r.get::<_, i64>(4)? != 0,
                    updated_at: r.get(5)?,
                })
            })
            .context("query list graphs")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect graphs")
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn set_node_state(&self, s: &NodeStateRow) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            r"INSERT INTO node_state (workspace_id, graph_id, node_id, state, attempt, started_at, finished_at, error)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT(workspace_id, graph_id, node_id) DO UPDATE SET
                 state = excluded.state, attempt = excluded.attempt,
                 started_at = excluded.started_at, finished_at = excluded.finished_at,
                 error = excluded.error",
            params![
                s.workspace_id,
                s.graph_id,
                s.node_id,
                s.state.to_db(),
                s.attempt,
                s.started_at,
                s.finished_at,
                s.error,
            ],
        )
        .context("set node state")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn list_node_states(
        &self,
        workspace_id: &str,
        graph_id: &str,
    ) -> Result<Vec<NodeStateRow>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT workspace_id, graph_id, node_id, state, attempt, started_at, finished_at, error
                 FROM node_state WHERE workspace_id = ?1 AND graph_id = ?2",
            )
            .context("prepare list node states")?;
        let rows = stmt
            .query_map(params![workspace_id, graph_id], |r| {
                Ok(NodeStateRow {
                    workspace_id: r.get(0)?,
                    graph_id: r.get(1)?,
                    node_id: r.get(2)?,
                    state: parse_node_state(&r.get::<_, String>(3)?),
                    attempt: u32::try_from(r.get::<_, i64>(4)?).unwrap_or(u32::MAX),
                    started_at: r.get(5)?,
                    finished_at: r.get(6)?,
                    error: r.get(7)?,
                })
            })
            .context("query list node states")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect node states")
    }

    // --- decisions --------------------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn append_decision(&self, d: &DecisionRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "INSERT INTO decision (id, signature, situation, decision, outcome, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                d.id,
                d.signature,
                serde_json::to_string(&d.situation).unwrap_or_default(),
                serde_json::to_string(&d.decision).unwrap_or_default(),
                d.outcome.as_ref().map(|o| serde_json::to_string(o).unwrap_or_default()),
                d.ts,
            ],
        )
        .context("append decision")?;
        Ok(())
    }

    /// Set a decision's outcome (M10) — the `outcome` column is JSON.
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn set_decision_outcome(&self, id: &str, outcome: &serde_json::Value) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "UPDATE decision SET outcome = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(outcome).unwrap_or_default()],
        )
        .context("set decision outcome")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn decisions_since(&self, ts: &str) -> Result<Vec<DecisionRecord>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, signature, situation, decision, outcome, ts
                 FROM decision WHERE ts >= ?1 ORDER BY ts",
            )
            .context("prepare decisions")?;
        let rows = stmt
            .query_map(params![ts], |r| {
                Ok(DecisionRecord {
                    id: r.get(0)?,
                    signature: r.get(1)?,
                    situation: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                    decision: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    outcome: r
                        .get::<_, Option<String>>(4)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    ts: r.get(5)?,
                })
            })
            .context("query decisions")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect decisions")
    }

    // --- rules ------------------------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn upsert_rule(&self, r: &StoredRule) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            r"INSERT INTO rule (id, toml, source, confidence, approved, active, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
               ON CONFLICT(id) DO UPDATE SET
                 toml = excluded.toml, source = excluded.source, confidence = excluded.confidence,
                 approved = excluded.approved, active = excluded.active",
            params![r.id, r.toml, r.source, r.confidence, r.approved, r.active, r.created_at],
        )
        .context("upsert rule")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn active_rules(&self) -> Result<Vec<StoredRule>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, toml, source, confidence, approved, active, created_at
                 FROM rule WHERE active = 1 ORDER BY id",
            )
            .context("prepare active rules")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StoredRule {
                    id: r.get(0)?,
                    toml: r.get(1)?,
                    source: r.get(2)?,
                    confidence: r.get(3)?,
                    approved: r.get::<_, i64>(4)? != 0,
                    active: r.get::<_, i64>(5)? != 0,
                    created_at: r.get(6)?,
                })
            })
            .context("query active rules")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect rules")
    }

    // --- proposals --------------------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn upsert_proposal(&self, p: &Proposal) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            r"INSERT INTO proposal (id, rule_toml, signature, cluster_size, confidence, status, created_at, resolved_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT(id) DO UPDATE SET
                 status = excluded.status, resolved_at = excluded.resolved_at",
            params![
                p.id,
                p.rule_toml,
                p.signature,
                i64::try_from(p.cluster_size).unwrap_or(i64::MAX),
                p.confidence,
                p.status.to_db(),
                p.created_at,
                p.resolved_at,
            ],
        )
        .context("upsert proposal")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn list_proposals(&self, status: Option<ProposalStatus>) -> Result<Vec<Proposal>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match status {
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, rule_toml, signature, cluster_size, confidence, status, created_at, resolved_at FROM proposal ORDER BY created_at",
                    )
                    .context("prepare list proposals")?;
                let rows = stmt
                    .query_map([], |r| Ok(proposal_from_row(r)))
                    .context("query list proposals")?;
                rows.collect::<rusqlite::Result<Vec<_>>>().context("collect proposals")
            }
            Some(st) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, rule_toml, signature, cluster_size, confidence, status, created_at, resolved_at FROM proposal WHERE status = ?1 ORDER BY created_at",
                    )
                    .context("prepare list proposals")?;
                let rows = stmt
                    .query_map(params![st.to_db()], |r| Ok(proposal_from_row(r)))
                    .context("query list proposals")?;
                rows.collect::<rusqlite::Result<Vec<_>>>().context("collect proposals")
            }
        }
    }

    // --- intake -----------------------------------------------------------

    /// # Errors
    /// Any `SQLite` failure.
    pub fn insert_intake(&self, item: &IntakeItem) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "INSERT OR REPLACE INTO intake (id, source, kind, title, body, severity, refs, graph_id, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                item.id,
                item.source,
                item.kind,
                item.title,
                item.body,
                item.severity,
                serde_json::to_string(&item.refs).unwrap_or_default(),
                item.graph_id,
                item.received_at,
            ],
        )
        .context("insert intake")?;
        Ok(())
    }

    /// Link an intake item to the workflow graph started for it (F3).
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn link_intake(&self, id: &str, graph_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute("UPDATE intake SET graph_id = ?2 WHERE id = ?1", params![id, graph_id])
            .context("link intake graph")?;
        Ok(())
    }

    /// # Errors
    /// Any `SQLite` failure.
    pub fn list_intake(&self, limit: usize) -> Result<Vec<IntakeItem>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, source, kind, title, body, severity, refs, graph_id, received_at
                 FROM intake ORDER BY received_at DESC LIMIT ?1",
            )
            .context("prepare list intake")?;
        let rows = stmt
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
                Ok(IntakeItem {
                    id: r.get(0)?,
                    source: r.get(1)?,
                    kind: r.get(2)?,
                    title: r.get(3)?,
                    body: r.get(4)?,
                    severity: r.get(5)?,
                    refs: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                    graph_id: r.get(7)?,
                    received_at: r.get(8)?,
                })
            })
            .context("query list intake")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect intake")
    }

    // --- usage (U5: token/cost collection) --------------------------------

    /// Insert a usage row (idempotent by `id`).
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn insert_usage(&self, u: &supervisor_core::types::UsageRow) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "INSERT OR IGNORE INTO usage (id, workspace_id, agent_id, model, ts, prompt_tokens, completion_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![u.id, u.workspace_id, u.agent_id, u.model, u.ts, i64::try_from(u.prompt_tokens).unwrap_or(i64::MAX), i64::try_from(u.completion_tokens).unwrap_or(i64::MAX)],
        )
        .context("insert usage")?;
        Ok(())
    }

    /// Usage rows, optionally filtered by workspace/agent and since a ts.
    ///
    /// # Errors
    /// Any `SQLite` failure.
    pub fn usage_since(
        &self,
        workspace: Option<&str>,
        agent: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<supervisor_core::types::UsageRow>> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Fixed ?1/?2/?3 with `IS NULL` short-circuits: a None filter binds
        // NULL and matches everything, so the placeholder count is always 3.
        let sql = "SELECT id, workspace_id, agent_id, model, ts, prompt_tokens, completion_tokens
                   FROM usage
                   WHERE (?1 IS NULL OR workspace_id = ?1)
                     AND (?2 IS NULL OR agent_id = ?2)
                     AND (?3 IS NULL OR ts >= ?3)
                   ORDER BY ts";
        let mut stmt = conn.prepare(sql).context("prepare usage")?;
        let rows = stmt
            .query_map(rusqlite::params![workspace, agent, since], |r| {
                Ok(supervisor_core::types::UsageRow {
                    id: r.get(0)?,
                    workspace_id: r.get(1)?,
                    agent_id: r.get(2)?,
                    model: r.get(3)?,
                    ts: r.get(4)?,
                    prompt_tokens: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(u64::MAX),
                    completion_tokens: u64::try_from(r.get::<_, i64>(6)?).unwrap_or(u64::MAX),
                })
            })
            .context("query usage")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect usage")
    }
}

fn workspace_from_row(r: &rusqlite::Row<'_>) -> Workspace {
    Workspace {
        id: r.get(0).unwrap_or_default(),
        path: r.get(1).unwrap_or_default(),
        port: r.get(2).unwrap_or_default(),
        server_pid: r
            .get::<_, Option<i64>>(3)
            .unwrap_or_default()
            .and_then(|p| u32::try_from(p).ok()),
        state: parse_workspace_state(&r.get::<_, String>(4).unwrap_or_default()),
        cmux_ws: r.get(5).unwrap_or_default(),
        layout_path: r.get(6).unwrap_or_default(),
        updated_at: r.get(7).unwrap_or_default(),
    }
}

fn agent_from_row(r: &rusqlite::Row<'_>) -> Agent {
    Agent {
        workspace_id: r.get(0).unwrap_or_default(),
        agent_id: r.get(1).unwrap_or_default(),
        role: r.get(2).unwrap_or_default(),
        model: r.get(3).unwrap_or_default(),
        session_id: r.get(4).unwrap_or_default(),
        driver: parse_driver(&r.get::<_, String>(5).unwrap_or_default()),
        mode: parse_mode(&r.get::<_, String>(6).unwrap_or_default()),
        state: parse_agent_state(&r.get::<_, String>(7).unwrap_or_default()),
        confidence: r.get(8).unwrap_or_default(),
    }
}

fn inbox_from_row(r: &rusqlite::Row<'_>) -> InboxEntry {
    InboxEntry {
        id: r.get(0).unwrap_or_default(),
        workspace_id: r.get(1).unwrap_or_default(),
        agent_id: r.get(2).unwrap_or_default(),
        priority: parse_priority(&r.get::<_, String>(3).unwrap_or_default()),
        body: r.get(4).unwrap_or_default(),
        from: r.get(5).unwrap_or_default(),
        kind: r.get(6).unwrap_or_default(),
        in_reply_to: r.get(7).unwrap_or_default(),
        ack_for: r.get(8).unwrap_or_default(),
        delivered: r.get::<_, i64>(9).unwrap_or_default() != 0,
        delivered_at: r.get(10).unwrap_or_default(),
        created_at: r.get(11).unwrap_or_default(),
    }
}

fn proposal_from_row(r: &rusqlite::Row<'_>) -> Proposal {
    Proposal {
        id: r.get(0).unwrap_or_default(),
        rule_toml: r.get(1).unwrap_or_default(),
        signature: r.get(2).unwrap_or_default(),
        cluster_size: r.get::<_, i64>(3).unwrap_or_default().try_into().unwrap_or_default(),
        confidence: r.get(4).unwrap_or_default(),
        status: parse_proposal_status(&r.get::<_, String>(5).unwrap_or_default()),
        created_at: r.get(6).unwrap_or_default(),
        resolved_at: r.get(7).unwrap_or_default(),
    }
}

// The DB stores snake_case strings; parse them back to enums (defaulting to a
// safe value on unknown input so a projection rebuild never fails).

#[allow(clippy::match_wildcard_for_single_variants)]
fn parse_workspace_state(s: &str) -> WorkspaceState {
    match s {
        "on" => WorkspaceState::On,
        "draining" => WorkspaceState::Draining,
        "error" => WorkspaceState::Error,
        _ => WorkspaceState::Off,
    }
}

fn parse_agent_state(s: &str) -> supervisor_core::types::AgentState {
    supervisor_core::types::AgentState::from_db(s)
}

fn parse_node_state(s: &str) -> NodeState {
    NodeState::from_db(s)
}

fn parse_driver(s: &str) -> supervisor_core::types::DriverKind {
    supervisor_core::types::DriverKind::from_db(s)
}

fn parse_mode(s: &str) -> supervisor_core::types::AgentMode {
    supervisor_core::types::AgentMode::from_db(s)
}

fn parse_priority(s: &str) -> supervisor_core::types::Priority {
    supervisor_core::types::Priority::from_db(s)
}

fn parse_proposal_status(s: &str) -> ProposalStatus {
    ProposalStatus::from_db(s)
}

/// The DB stores `snake_case` strings; `to_db`/`from_db` keep the conversion in
/// one place and default to a safe value on unknown input so a projection
/// rebuild never fails.
pub(crate) trait DbCodec: Sized {
    fn to_db(self) -> &'static str;
    fn from_db(s: &str) -> Self;
}

impl DbCodec for WorkspaceState {
    fn to_db(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Draining => "draining",
            Self::Error => "error",
            Self::Off => "off",
        }
    }

    #[allow(clippy::match_wildcard_for_single_variants)]
    fn from_db(s: &str) -> Self {
        match s {
            "on" => Self::On,
            "draining" => Self::Draining,
            "error" => Self::Error,
            _ => Self::Off,
        }
    }
}

impl DbCodec for supervisor_core::types::AgentMode {
    fn to_db(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
        }
    }

    fn from_db(s: &str) -> Self {
        if s == "background" { Self::Background } else { Self::Foreground }
    }
}

impl DbCodec for supervisor_core::types::DriverKind {
    fn to_db(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Cmux => "cmux",
        }
    }

    fn from_db(s: &str) -> Self {
        if s == "cmux" { Self::Cmux } else { Self::Opencode }
    }
}

impl DbCodec for supervisor_core::types::AgentState {
    fn to_db(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Spawning => "spawning",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::WaitingInput => "waiting_input",
            Self::BlockedPermission => "blocked_permission",
            Self::Error => "error",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "spawning" => Self::Spawning,
            "working" => Self::Working,
            "idle" => Self::Idle,
            "waiting_input" => Self::WaitingInput,
            "blocked_permission" => Self::BlockedPermission,
            "error" => Self::Error,
            _ => Self::Unknown,
        }
    }
}

impl DbCodec for NodeState {
    fn to_db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::NeedsDecision => "needs_decision",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "ready" => Self::Ready,
            "running" => Self::Running,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "needs_decision" => Self::NeedsDecision,
            _ => Self::Pending,
        }
    }
}

impl DbCodec for supervisor_core::types::Priority {
    fn to_db(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    fn from_db(s: &str) -> Self {
        if s == "high" { Self::High } else { Self::Normal }
    }
}

impl DbCodec for ProposalStatus {
    fn to_db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "applied" => Self::Applied,
            "rejected" => Self::Rejected,
            "expired" => Self::Expired,
            _ => Self::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use supervisor_core::types::{AgentState, Priority, WorkspaceState};

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn ws(id: &str) -> Workspace {
        Workspace {
            id: id.to_owned(),
            path: format!("/x/{id}"),
            port: Some(4101),
            server_pid: Some(1234),
            state: WorkspaceState::On,
            cmux_ws: Some("ws_1".to_owned()),
            layout_path: None,
            updated_at: "2026-08-13T00:00:00.000Z".to_owned(),
        }
    }

    fn agent(ws: &str, id: &str) -> Agent {
        Agent {
            workspace_id: ws.to_owned(),
            agent_id: id.to_owned(),
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
    fn workspace_roundtrip() {
        let s = store();
        s.upsert_workspace(&ws("iot")).unwrap();
        assert_eq!(s.get_workspace("iot").unwrap().unwrap().id, "iot");
        assert_eq!(s.get_workspace("ghost").unwrap(), None);
        assert_eq!(s.list_workspaces().unwrap().len(), 1);
        let mut updated = ws("iot");
        updated.state = WorkspaceState::Draining;
        s.upsert_workspace(&updated).unwrap();
        assert_eq!(s.get_workspace("iot").unwrap().unwrap().state, WorkspaceState::Draining);
    }

    #[test]
    fn agent_roundtrip() {
        let s = store();
        s.upsert_workspace(&ws("iot")).unwrap();
        s.upsert_agent(&agent("iot", "dev_01")).unwrap();
        let agents = s.list_agents("iot").unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state, AgentState::Idle);
        assert_eq!(agents[0].session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn inbox_claims_in_priority_order() {
        let s = store();
        let entry = |id: &str, priority: Priority, created: &str| InboxEntry {
            id: id.to_owned(),
            workspace_id: "iot".to_owned(),
            agent_id: "dev_01".to_owned(),
            priority,
            body: format!("body {id}"),
            from: "human".to_owned(),
            kind: "instruction".to_owned(),
            in_reply_to: None,
            ack_for: None,
            delivered: false,
            delivered_at: None,
            created_at: created.to_owned(),
        };
        s.enqueue_inbox(&entry("a", Priority::Normal, "2026-08-13T00:00:00.000Z")).unwrap();
        s.enqueue_inbox(&entry("b", Priority::High, "2026-08-13T00:00:01.000Z")).unwrap();
        s.enqueue_inbox(&entry("c", Priority::Normal, "2026-08-13T00:00:02.000Z")).unwrap();

        let claimed = s.claim_inbox("iot", "dev_01", 10).unwrap();
        let ids: Vec<&str> = claimed.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a", "c"], "high first, then FIFO");
        assert_eq!(s.inbox_depth("iot", "dev_01").unwrap(), 3);

        s.mark_delivered("a", "2026-08-13T00:01:00.000Z").unwrap();
        let remaining = s.claim_inbox("iot", "dev_01", 10).unwrap();
        assert_eq!(remaining.len(), 2, "delivered entry no longer claimed");
        assert_eq!(s.inbox_depth("iot", "dev_01").unwrap(), 2);
    }

    #[test]
    fn graph_and_node_state_roundtrip() {
        let s = store();
        let g = Graph {
            id: "feature_lifecycle".to_owned(),
            name: "n".to_owned(),
            data: r#"{"id":"feature_lifecycle","name":"n","nodes":[]}"#.to_owned(),
            version: 1,
            active: true,
            updated_at: "t".to_owned(),
        };
        s.upsert_graph(&g).unwrap();
        assert_eq!(s.get_graph("feature_lifecycle").unwrap().unwrap().id, "feature_lifecycle");
        let node = NodeStateRow {
            workspace_id: "iot".to_owned(),
            graph_id: "feature_lifecycle".to_owned(),
            node_id: "dev".to_owned(),
            state: NodeState::Running,
            attempt: 1,
            started_at: Some("t".to_owned()),
            finished_at: None,
            error: None,
        };
        s.set_node_state(&node).unwrap();
        let states = s.list_node_states("iot", "feature_lifecycle").unwrap();
        assert_eq!(states[0].state, NodeState::Running);
        assert_eq!(states[0].attempt, 1);
    }

    #[test]
    fn decisions_and_rules_roundtrip() {
        let s = store();
        let d = DecisionRecord {
            id: "d1".to_owned(),
            signature: "role=dev".to_owned(),
            situation: serde_json::json!({"agent_role":"dev"}),
            decision: serde_json::json!({"kind":"post"}),
            outcome: Some(serde_json::json!({"success": true})),
            ts: "2026-08-13T00:00:00.000Z".to_owned(),
        };
        s.append_decision(&d).unwrap();
        let rows = s.decisions_since("2020-01-01T00:00:00.000Z").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome.as_ref().unwrap()["success"], true);
        assert!(s.decisions_since("2999-01-01T00:00:00.000Z").unwrap().is_empty());

        let rule = StoredRule {
            id: "r1".to_owned(),
            toml: "[[rule]]\nid = \"r1\"\n".to_owned(),
            source: "data".to_owned(),
            confidence: 0.9,
            approved: true,
            active: true,
            created_at: "t".to_owned(),
        };
        s.upsert_rule(&rule).unwrap();
        assert_eq!(s.active_rules().unwrap().len(), 1);
    }

    #[test]
    fn proposals_and_intake_roundtrip() {
        let s = store();
        let p = Proposal {
            id: "proposal_1".to_owned(),
            rule_toml: "[[rule]]\n".to_owned(),
            signature: "sig".to_owned(),
            cluster_size: 3,
            confidence: 0.8,
            status: ProposalStatus::Pending,
            created_at: "t".to_owned(),
            resolved_at: None,
        };
        s.upsert_proposal(&p).unwrap();
        assert_eq!(s.list_proposals(None).unwrap().len(), 1);
        assert_eq!(s.list_proposals(Some(ProposalStatus::Applied)).unwrap().len(), 0);
        let mut applied = p.clone();
        applied.status = ProposalStatus::Applied;
        applied.resolved_at = Some("t2".to_owned());
        s.upsert_proposal(&applied).unwrap();
        assert_eq!(s.list_proposals(Some(ProposalStatus::Applied)).unwrap().len(), 1);

        let item = IntakeItem {
            id: "i1".to_owned(),
            source: "github".to_owned(),
            kind: "bug".to_owned(),
            title: "t".to_owned(),
            body: "b".to_owned(),
            severity: Some("high".to_owned()),
            refs: vec!["ref".to_owned()],
            graph_id: Some("bug_flow".to_owned()),
            received_at: "t".to_owned(),
        };
        s.insert_intake(&item).unwrap();
        let items = s.list_intake(10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].refs, vec!["ref".to_owned()]);
    }

    #[test]
    fn ports_roundtrip() {
        let s = store();
        s.upsert_workspace(&ws("iot")).unwrap();
        s.upsert_port(&PortRow {
            port: 4101,
            workspace_id: "iot".to_owned(),
            allocated_at: "t".to_owned(),
        })
        .unwrap();
        assert_eq!(s.list_ports().unwrap().len(), 1);
        s.delete_port(4101).unwrap();
        assert!(s.list_ports().unwrap().is_empty());
    }

    #[test]
    fn rebuild_drops_and_recreates() {
        let s = store();
        s.upsert_workspace(&ws("iot")).unwrap();
        s.rebuild().unwrap();
        assert!(s.list_workspaces().unwrap().is_empty());
        s.upsert_workspace(&ws("iot")).unwrap();
        assert_eq!(s.list_workspaces().unwrap().len(), 1);
    }
}
