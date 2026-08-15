//! Domain value types shared by the core and the daemon.
//!
//! These are plain serializable data shapes (mirroring the `SQLite` schema in
//! `docs/specs/2026-08-13-supervisor-detailed-design.md` §3.1). The daemon
//! persists them via the journal and the `SQLite` projection; the core reasons
//! about them without touching a socket or a file.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A workspace slug, e.g. `iot_platform`.
pub type WorkspaceId = String;

/// An agent id within a workspace, e.g. `dev_01`.
pub type AgentId = String;

/// An opencode session id.
pub type SessionId = String;

/// The lifecycle state of a workspace (`off → on → draining → off`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// Registered but not running: no server, no panes.
    #[default]
    Off,
    /// Server running, panes attached, agents driveable.
    On,
    /// Graceful teardown in progress: waiting for turns to finish.
    Draining,
    /// The workspace's server crashed or the last operation failed.
    Error,
}

/// Whether an agent gets a foreground cmux pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// A cmux terminal surface per agent, attached to the shared `serve`.
    #[default]
    Foreground,
    /// Headless on the server; no pane until `supervisor attach`.
    Background,
}

/// Which harness drives an agent. Only `Opencode` is implemented today; `Cmux`
/// drives pane-typed harnesses (Claude Code, Pi, Codex) and is future work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    #[default]
    Opencode,
    Cmux,
}

/// An agent's observed condition (`unknown → spawning → working → idle`, with
/// pauses and error).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Never seen, or the session is unknown right now.
    #[default]
    Unknown,
    /// Server is booting / session being attached.
    Spawning,
    /// Running a turn — output is moving.
    Working,
    /// Finished a turn with no pending background work.
    Idle,
    /// Finished a turn and is waiting for human/manager input to continue.
    WaitingInput,
    /// Waiting for a tool-permission approval.
    BlockedPermission,
    /// Crashed, stepped failed, or matched error markers.
    Error,
}

/// The status opencode reports per session (`GET /session/status`).
///
/// Note: idle sessions are *omitted* from the status map (verified); idle
/// arrives only on SSE. The enum keeps `Idle` for parsing / status polls of a
/// known session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Idle,
    Busy,
    Retry,
}

/// The lifecycle state of one workflow node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Not ready yet: a dependency is still unfinished.
    #[default]
    Pending,
    /// Dependencies are done; the engine may start it.
    Ready,
    /// A start message has been delivered; the owning agent is working on it.
    Running,
    /// Blocked on something outside the DAG (e.g. a missing role).
    Blocked,
    /// The node's `done_when` fired.
    Done,
    /// The node errored past its rerun bound.
    Failed,
    /// Completion is ambiguous; the manager or a human must rule.
    NeedsDecision,
}

/// The status an agent reports in its ACK contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    Done,
    Failed,
    Blocked,
}

/// Human-gate feedback size; selects the `loop_back` target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Revision {
    #[default]
    None,
    Small,
    Medium,
    Big,
}

/// Delivery hint for an inbox entry. High is pulled ahead of normal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    #[default]
    Normal,
    High,
}

/// A registered workspace (the `workspace` table row).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub id: WorkspaceId,
    /// Absolute project directory.
    pub path: String,
    /// opencode server port; `None` when off.
    pub port: Option<u16>,
    /// The PID of the recorded `opencode serve`, for adopt-or-kill (§4.3);
    /// `None` when off or unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_pid: Option<u32>,
    #[serde(default)]
    pub state: WorkspaceState,
    /// cmux workspace handle, when the workspace is `on`.
    pub cmux_ws: Option<String>,
    /// `supervisor.toml` path (project-local), when known.
    pub layout_path: Option<String>,
    /// RFC 3339 timestamp of the last mutation.
    pub updated_at: String,
}

impl Workspace {
    /// The process id of the recorded opencode server, if one is recorded.
    #[must_use]
    pub fn recorded_port(&self) -> Option<u16> {
        self.port
    }

    /// The recorded server PID, if any.
    #[must_use]
    pub fn recorded_pid(&self) -> Option<u32> {
        self.server_pid
    }
}

/// An agent row (the `agent` table). `confidence` 1.0 means observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub role: String,
    pub model: Option<String>,
    pub session_id: Option<SessionId>,
    /// The harness that drives this agent (§4.7); opencode by default.
    #[serde(default)]
    pub driver: DriverKind,
    /// `foreground` (pane) or `background` (headless); from the roster.
    #[serde(default)]
    pub mode: AgentMode,
    #[serde(default)]
    pub state: AgentState,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
}

fn default_confidence() -> f64 {
    1.0
}

/// A roster agent as configured in `supervisor.toml` (its `[[agent]]` entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RosterAgent {
    pub id: AgentId,
    pub role: String,
    pub model: Option<String>,
    #[serde(default)]
    pub driver: DriverKind,
    #[serde(default)]
    pub mode: AgentMode,
}

/// One inbox entry (the `inbox_entry` table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxEntry {
    /// ULID, monotonic and sortable.
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    #[serde(default)]
    pub priority: Priority,
    pub body: String,
    /// `"human"`, `"workflow"`, or an agent id.
    #[serde(default)]
    pub from: String,
    /// `instruction` by default.
    #[serde(default)]
    pub kind: String,
    pub in_reply_to: Option<String>,
    /// The task id this entry acks, if it is an ack.
    pub ack_for: Option<String>,
    #[serde(default)]
    pub delivered: bool,
    pub delivered_at: Option<String>,
    pub created_at: String,
}

/// An ingested item (the `intake` table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntakeItem {
    pub id: String,
    /// `github` | `app-feedback` | `cli`.
    pub source: String,
    /// `bug` | `feature` | `feedback`.
    pub kind: String,
    pub title: String,
    pub body: String,
    pub severity: Option<String>,
    /// JSON array of references.
    #[serde(default)]
    pub refs: Vec<String>,
    /// The workflow started for this item, once one is.
    pub graph_id: Option<String>,
    pub received_at: String,
}

impl IntakeItem {
    /// The workflow graph for this item's kind, if any (review finding 2:
    /// both intake paths used to start with empty vars, so prompts carried
    /// literal `{bug}`/`{feature}`/`{spec}` placeholders).
    #[must_use]
    pub fn workflow_graph(&self) -> Option<&'static str> {
        match self.kind.as_str() {
            "bug" => Some("bug_flow"),
            "feature" => Some("feature_lifecycle"),
            _ => None,
        }
    }

    /// Workflow render variables for this item: the template placeholders
    /// (`{bug}`/`{feature}`/`{spec}`/`{title}`/`{body}`/`{severity}`) filled
    /// from the item's fields. Empty for kinds with no workflow.
    #[must_use]
    pub fn workflow_vars(&self) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();
        let key = match self.kind.as_str() {
            "bug" => "bug",
            "feature" => "feature",
            _ => return vars,
        };
        vars.insert(key.to_owned(), self.title.clone());
        vars.insert("spec".to_owned(), self.body.clone());
        vars.insert("title".to_owned(), self.title.clone());
        vars.insert("body".to_owned(), self.body.clone());
        if let Some(severity) = &self.severity {
            vars.insert("severity".to_owned(), severity.clone());
        }
        vars
    }
}

/// A decision row (the `decision` table). The outcome is filled in later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecord {
    pub id: String,
    /// Normalized situation signature (see [`crate::bakeback`]).
    pub signature: String,
    /// JSON snapshot of the situation.
    pub situation: serde_json::Value,
    /// JSON action that was taken.
    pub decision: serde_json::Value,
    /// JSON result, filled later by the outcome observer.
    pub outcome: Option<serde_json::Value>,
    pub ts: String,
}

/// A bake-back proposal (the `proposal` table). Id is stable across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    /// `proposal_<ulid>`.
    pub id: String,
    pub rule_toml: String,
    pub signature: String,
    pub cluster_size: usize,
    pub confidence: f64,
    #[serde(default)]
    pub status: ProposalStatus,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

/// The lifecycle of a bake-back proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    #[default]
    Pending,
    Applied,
    Rejected,
    Expired,
}

/// A stored rule (the `rule` table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRule {
    pub id: String,
    /// The full `[[rule]]` TOML block.
    pub toml: String,
    /// `data` | `code` | `bakeback`.
    pub source: String,
    pub confidence: f64,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub active: bool,
    pub created_at: String,
}

/// A workflow graph as stored (the `graph` table). `data` is the JSON
/// `{id, name, nodes}` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    pub id: String,
    pub name: String,
    pub data: String,
    #[serde(default = "default_one")]
    pub version: u32,
    #[serde(default = "default_true")]
    pub active: bool,
    pub updated_at: String,
}

fn default_one() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

/// A node-state row (the `node_state` table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeStateRow {
    /// I-1 (review): node state is keyed per workspace — two workspaces
    /// running the same graph must not corrupt each other's rows. `default`
    /// keeps pre-I-1 journal records replayable.
    #[serde(default)]
    pub workspace_id: String,
    pub graph_id: String,
    pub node_id: String,
    #[serde(default)]
    pub state: NodeState,
    #[serde(default)]
    pub attempt: u32,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

/// A port allocation row (the `port` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortRow {
    pub port: u16,
    pub workspace_id: WorkspaceId,
    pub allocated_at: String,
}

/// One usage row (the `usage` table, §3.3). Tokens are stored; cost is
/// computed on read from `model_prices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRow {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub model: Option<String>,
    pub ts: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::float_cmp,
        clippy::match_single_binding,
        clippy::wildcard_in_or_patterns,
        clippy::cast_precision_loss
    )]
    use super::*;

    #[test]
    fn workspace_roundtrips_through_json() {
        let ws = Workspace {
            id: "iot_platform".to_owned(),
            path: "/Users/u/development/iot_platform".to_owned(),
            port: Some(4101),
            server_pid: Some(4242),
            state: WorkspaceState::On,
            cmux_ws: Some("ws_01".to_owned()),
            layout_path: Some("supervisor.toml".to_owned()),
            updated_at: "2026-08-13T00:00:00.000Z".to_owned(),
        };
        let back: Workspace = serde_json::from_str(&serde_json::to_string(&ws).unwrap()).unwrap();
        assert_eq!(back, ws);
        assert_eq!(back.recorded_port(), Some(4101));
    }

    #[test]
    fn agent_defaults_are_unknown_and_confident() {
        let a: Agent =
            serde_json::from_str(r#"{"workspace_id":"iot","agent_id":"dev_01","role":"dev"}"#)
                .unwrap();
        assert_eq!(a.state, AgentState::Unknown);
        assert_eq!(a.confidence, 1.0);
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(serde_json::to_string(&AgentState::WaitingInput).unwrap(), r#""waiting_input""#);
        assert_eq!(serde_json::to_string(&WorkspaceState::Draining).unwrap(), r#""draining""#);
        assert_eq!(
            serde_json::to_string(&NodeState::NeedsDecision).unwrap(),
            r#""needs_decision""#
        );
    }

    #[test]
    fn workspace_state_defaults_off() {
        assert_eq!(WorkspaceState::default(), WorkspaceState::Off);
    }

    #[test]
    fn intake_item_maps_kind_to_graph_and_vars() {
        // Review finding 2: intake items must feed {bug}/{feature}/{spec} so
        // prompts never carry literal placeholders.
        let bug = IntakeItem {
            id: "in_1".to_owned(),
            source: "github".to_owned(),
            kind: "bug".to_owned(),
            title: "crash on login".to_owned(),
            body: "steps to reproduce".to_owned(),
            severity: Some("high".to_owned()),
            refs: vec![],
            graph_id: None,
            received_at: "t".to_owned(),
        };
        assert_eq!(bug.workflow_graph(), Some("bug_flow"));
        let vars = bug.workflow_vars();
        assert_eq!(vars.get("bug").map(String::as_str), Some("crash on login"));
        assert_eq!(vars.get("spec").map(String::as_str), Some("steps to reproduce"));
        assert_eq!(vars.get("severity").map(String::as_str), Some("high"));

        let feature = IntakeItem { kind: "feature".to_owned(), ..bug.clone() };
        assert_eq!(feature.workflow_graph(), Some("feature_lifecycle"));
        assert_eq!(
            feature.workflow_vars().get("feature").map(String::as_str),
            Some("crash on login")
        );

        let feedback = IntakeItem { kind: "feedback".to_owned(), ..bug };
        assert_eq!(feedback.workflow_graph(), None);
        assert!(feedback.workflow_vars().is_empty());
    }
}
