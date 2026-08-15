//! The workflow engine (C10): declarative DAGs over agents (§4.11).
//!
//! A [`Workflow`] is a DAG of [`NodeDef`]s. Nodes carry a role (resolved to an
//! agent via the roster), a `start_template`, a [`DoneWhen`] criterion, an
//! `on_error` policy, and — for human gates — a [`LoopBack`] mapping
//! `needs_revision` to a target. The engine is pure and offline: it advances
//! node states on acks / matches / failures / timeouts / manager rulings and
//! reports [`WorkflowEvent`]s the daemon turns into inbox deliveries.
//!
//! Role → agent resolution follows the spec's order: explicit `agent_id`, else
//! the least-loaded idle matching-role agent, else the first matching-role
//! agent, else `MissingRole` (the node holds and the dashboard surfaces it).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::ack::Ack;
use crate::error::{CoreError, CoreResult};
use crate::types::{AckStatus, AgentId, AgentMode, AgentState, NodeState, Revision, SessionId};

/// One task in a workflow graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDef {
    pub id: String,
    /// The role that owns this task; resolved to an agent via the roster.
    pub role: String,
    /// An explicit agent override; step (1) of role resolution.
    pub agent_id: Option<AgentId>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The instruction to deliver when the node starts. `{key}` placeholders
    /// render workflow variables.
    pub start_template: String,
    #[serde(default)]
    pub done_when: DoneWhen,
    #[serde(default)]
    pub on_error: OnError,
    /// `plannotator` for human-gate nodes.
    pub gate: Option<String>,
    /// For human-gate nodes: maps `needs_revision` to a re-run target.
    pub loop_back: Option<LoopBack>,
    /// `foreground` (default) or `background` (no pane).
    #[serde(default)]
    pub mode: AgentMode,
    /// Per-node timeout before a running node moves to `needs_decision`.
    pub timeout_secs: Option<u64>,
}

impl Default for NodeDef {
    fn default() -> Self {
        Self {
            id: String::new(),
            role: String::new(),
            agent_id: None,
            depends_on: Vec::new(),
            start_template: String::new(),
            done_when: DoneWhen::default(),
            on_error: OnError::default(),
            gate: None,
            loop_back: None,
            mode: AgentMode::Foreground,
            timeout_secs: None,
        }
    }
}

/// How a node proves it finished (§4.9, §4.11).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoneWhen {
    /// The `task_id` an ACK must carry to complete this node.
    pub ack: Option<String>,
    /// A human gate: only an ACK with `approved: true` completes the node.
    pub approved: Option<bool>,
    /// A regex over the last output (test banners etc.); matching completes
    /// the node.
    #[serde(rename = "match", default)]
    pub r#match: Option<String>,
}

impl DoneWhen {
    /// Is this a human-gate node?
    #[must_use]
    pub fn is_human_gate(&self) -> bool {
        self.approved.is_some()
    }

    /// Has at least one completion criterion?
    #[must_use]
    pub fn has_criterion(&self) -> bool {
        self.ack.is_some() || self.r#match.is_some()
    }
}

/// Where a human-gate loop sends the work, keyed on `needs_revision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopBack {
    /// Always `needs_revision` today; kept for forward-compat.
    #[serde(default = "default_loop_on")]
    pub on: String,
    /// `small` / `medium` feedback → back to the gate node (stays in the human
    /// loop).
    pub small: String,
    /// `big` feedback → back to an earlier agent-review node (re-run review).
    pub big: String,
}

fn default_loop_on() -> String {
    "needs_revision".to_owned()
}

/// The `on_error` policy of a node.
///
/// Serialized in the graph's JSON forms: a bare string (`"delegate"` / `"skip"`)
/// or an object (`{ "rerun": { "max": 2 } }`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OnError {
    /// Re-deliver the start message, up to `max` extra attempts.
    Rerun { max: u8 },
    /// Mark done anyway (e.g. cosmetic nodes).
    Skip,
    /// Hand the ruling to the manager (C11).
    #[default]
    Delegate,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OnErrorRaw {
    Kind(String),
    Rerun { rerun: RerunSpec },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RerunSpec {
    max: u8,
}

impl<'de> Deserialize<'de> for OnError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        match OnErrorRaw::deserialize(deserializer)? {
            OnErrorRaw::Kind(kind) => match kind.as_str() {
                "delegate" => Ok(Self::Delegate),
                "skip" => Ok(Self::Skip),
                other => Err(D::Error::custom(format!("unknown on_error kind {other:?}"))),
            },
            OnErrorRaw::Rerun { rerun } => Ok(Self::Rerun { max: rerun.max }),
        }
    }
}

impl Serialize for OnError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            Self::Delegate => serializer.serialize_str("delegate"),
            Self::Skip => serializer.serialize_str("skip"),
            Self::Rerun { max } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("rerun", &serde_json::json!({ "max": max }))?;
                map.end()
            }
        }
    }
}

impl OnError {
    #[must_use]
    pub fn max_reruns(&self) -> u8 {
        match self {
            Self::Rerun { max } => *max,
            Self::Skip | Self::Delegate => 0,
        }
    }
}

/// A workflow graph definition (`{id, name, nodes}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphDef {
    pub id: String,
    pub name: String,
    pub nodes: Vec<NodeDef>,
}

/// The manager's ruling on a `NeedsDecision` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerRuling {
    Done,
    Rerun,
    Skip,
    /// Split into subtasks — not modeled by the engine; the node blocks until
    /// the human gives a concrete plan.
    Split,
}

impl std::str::FromStr for ManagerRuling {
    type Err = ();

    /// Parse a ruling action string (`done|rerun|skip|split`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "done" => Ok(Self::Done),
            "rerun" => Ok(Self::Rerun),
            "skip" => Ok(Self::Skip),
            "split" => Ok(Self::Split),
            _ => Err(()),
        }
    }
}

/// A node lifecycle event, for the daemon to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkflowEvent {
    /// All dependencies done; a start message should be delivered.
    NodeReady { graph: String, node: String },
    /// The daemon delivered the start message.
    NodeStarted { graph: String, node: String },
    /// The node completed.
    NodeDone { graph: String, node: String, skipped: bool },
    /// The node failed past its rerun bound.
    NodeFailed { graph: String, node: String },
    /// The node is blocked on something outside the DAG.
    NodeBlocked { graph: String, node: String, reason: String },
    /// Completion is ambiguous; the manager must rule.
    NodeNeedsDecision { graph: String, node: String },
    /// A resolved ACK arrived (published for the decision log / observers).
    Ack { graph: String, ack: Ack },
    /// A human gate looped back to a target with a revision size.
    LoopBack { graph: String, node: String, target: String, revision: Revision },
    /// A ready node has no agent with its role.
    MissingRole { graph: String, node: String, role: String },
}

/// A roster entry snapshot used by role → agent resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RosterEntry {
    pub agent_id: AgentId,
    pub role: String,
    pub state: AgentState,
    pub session_id: Option<SessionId>,
    pub inbox_depth: usize,
}

/// The result of resolving a node's role to an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoleResolution {
    Target(AgentId),
    MissingRole { role: String },
}

/// A running workflow instance. State lives here and only here; a fresh
/// instance of the same definition starts from `Pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    graph: GraphDef,
    states: BTreeMap<String, NodeState>,
    reruns: BTreeMap<String, u8>,
    attempts: BTreeMap<String, u32>,
}

impl Workflow {
    /// Validate and build a workflow.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidGraph`] for duplicate/empty ids, a
    /// dependency on an unknown node, a cycle, a node with no completion
    /// criterion, or a human gate whose `loop_back` targets do not exist.
    pub fn new(graph: GraphDef) -> CoreResult<Self> {
        let err = |reason: String| CoreError::InvalidGraph { id: graph.id.clone(), reason };
        let mut ids = HashSet::new();
        for node in &graph.nodes {
            if node.id.is_empty() {
                return Err(err("a node id must not be empty".to_owned()));
            }
            if !ids.insert(node.id.clone()) {
                return Err(err(format!("duplicate node id {:?}", node.id)));
            }
            if !node.done_when.has_criterion() {
                return Err(err(format!(
                    "node {:?} has no done_when criterion (ack or match)",
                    node.id
                )));
            }
        }
        for node in &graph.nodes {
            for dep in &node.depends_on {
                if !ids.contains(dep) {
                    return Err(err(format!(
                        "node {:?} depends on unknown node {:?}",
                        node.id, dep
                    )));
                }
            }
            if let Some(lb) = &node.loop_back {
                for target in [&lb.small, &lb.big] {
                    if !ids.contains(target.as_str()) {
                        return Err(err(format!(
                            "node {:?} loop_back target {:?} does not exist",
                            node.id, target
                        )));
                    }
                }
            }
        }
        if let Some(cycle) = find_cycle(&graph.nodes) {
            return Err(err(format!("dependency cycle: {cycle:?}")));
        }

        let states = graph
            .nodes
            .iter()
            .map(|n| {
                (
                    n.id.clone(),
                    if n.depends_on.is_empty() { NodeState::Ready } else { NodeState::Pending },
                )
            })
            .collect();
        Ok(Self { graph, states, reruns: BTreeMap::new(), attempts: BTreeMap::new() })
    }

    /// Parse a graph JSON document (`{id, name, nodes}`).
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidGraph`] for schema or graph problems.
    pub fn parse_json(data: &str) -> CoreResult<Self> {
        let graph: GraphDef = serde_json::from_str(data).map_err(|e| CoreError::InvalidGraph {
            id: "<json>".to_owned(),
            reason: e.to_string(),
        })?;
        Self::new(graph)
    }

    #[must_use]
    pub fn graph(&self) -> &GraphDef {
        &self.graph
    }

    #[must_use]
    pub fn node(&self, id: &str) -> Option<&NodeDef> {
        self.graph.nodes.iter().find(|n| n.id == id)
    }

    #[must_use]
    pub fn nodes(&self) -> &[NodeDef] {
        &self.graph.nodes
    }

    #[must_use]
    pub fn state(&self, id: &str) -> Option<NodeState> {
        self.states.get(id).copied()
    }

    /// The states of every node, in definition order, for `dag status`.
    #[must_use]
    pub fn states(&self) -> Vec<(&str, NodeState)> {
        self.graph
            .nodes
            .iter()
            .filter_map(|n| self.states.get(&n.id).map(|s| (n.id.as_str(), *s)))
            .collect()
    }

    /// True when every node is `Done` (including skipped).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.graph.nodes.iter().all(|n| self.states[&n.id] == NodeState::Done)
    }

    /// The nodes currently in `Ready`, in definition order. After a restart
    /// this is the resume list.
    #[must_use]
    pub fn ready(&self) -> Vec<&NodeDef> {
        self.graph.nodes.iter().filter(|n| self.states[&n.id] == NodeState::Ready).collect()
    }

    /// The nodes currently `Running`, in definition order.
    #[must_use]
    pub fn running(&self) -> Vec<&NodeDef> {
        self.graph.nodes.iter().filter(|n| self.states[&n.id] == NodeState::Running).collect()
    }

    /// The per-node timeout, if configured.
    #[must_use]
    pub fn node_timeout(&self, id: &str) -> Option<u64> {
        self.node(id).and_then(|n| n.timeout_secs)
    }

    /// Rerun attempts so far for a node.
    #[must_use]
    pub fn reruns(&self, id: &str) -> u8 {
        self.reruns.get(id).copied().unwrap_or(0)
    }

    /// Restore persisted node states after a daemon restart (M3). Running
    /// nodes become `Ready` — the start message is re-delivered and the
    /// agent's task-id idempotency absorbs the duplicate; `Done`/`Failed`/
    /// `Blocked`/`NeedsDecision` are kept.
    pub fn restore_states(&mut self, states: impl IntoIterator<Item = (String, NodeState)>) {
        for (node, state) in states {
            let state = if state == NodeState::Running { NodeState::Ready } else { state };
            if self.states.contains_key(&node) {
                self.states.insert(node, state);
            }
        }
    }

    /// Start attempts so far for a node.
    #[must_use]
    pub fn attempts(&self, id: &str) -> u32 {
        self.attempts.get(id).copied().unwrap_or(0)
    }

    /// Render a node's start instruction against workflow variables.
    #[must_use]
    pub fn render_start(&self, id: &str, vars: &BTreeMap<String, String>) -> Option<String> {
        let node = self.node(id)?;
        let mut rendered = node.start_template.clone();
        for (key, value) in vars {
            rendered = rendered.replace(&format!("{{{key}}}"), value);
        }
        Some(rendered)
    }

    /// Mark a ready node as started (the daemon delivers its start message at
    /// the same time).
    #[must_use]
    pub fn start(&mut self, id: &str) -> Option<WorkflowEvent> {
        if self.states.get(id)? != &NodeState::Ready {
            return None;
        }
        self.states.insert(id.to_owned(), NodeState::Running);
        *self.attempts.entry(id.to_owned()).or_insert(0) += 1;
        Some(WorkflowEvent::NodeStarted { graph: self.graph.id.clone(), node: id.to_owned() })
    }

    /// Resolve a node's role to an agent (§4.11 role → agent resolution).
    #[must_use]
    pub fn resolve_target(&self, node_id: &str, roster: &[RosterEntry]) -> RoleResolution {
        let Some(node) = self.node(node_id) else {
            return RoleResolution::MissingRole { role: String::new() };
        };
        if let Some(agent) = &node.agent_id {
            return RoleResolution::Target(agent.clone());
        }
        let matching: Vec<&RosterEntry> = roster.iter().filter(|a| a.role == node.role).collect();
        if matching.is_empty() {
            return RoleResolution::MissingRole { role: node.role.clone() };
        }
        let idle =
            matching.iter().filter(|a| a.state == AgentState::Idle).copied().collect::<Vec<_>>();
        if let Some(best) = idle.iter().min_by_key(|a| (a.inbox_depth, a.session_id.clone())) {
            return RoleResolution::Target(best.agent_id.clone());
        }
        RoleResolution::Target(matching[0].agent_id.clone())
    }

    /// Ready nodes whose role currently has no agent, for the missing-role
    /// policy (holds + logged; the dashboard surfaces them).
    #[must_use]
    pub fn missing_roles(&self, roster: &[RosterEntry]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for node in self.ready() {
            if let RoleResolution::MissingRole { role } = self.resolve_target(&node.id, roster) {
                out.push((node.id.clone(), role));
            }
        }
        out
    }

    /// Apply an ack (§4.9): complete matching running nodes, apply human-gate
    /// semantics (`approved` + `loop_back`), and route failed/blocked acks
    /// through `on_error`. Returns the events to act on, including any newly
    /// ready nodes.
    #[must_use]
    pub fn apply_ack(&mut self, ack: &Ack) -> Vec<WorkflowEvent> {
        let mut events =
            vec![WorkflowEvent::Ack { graph: self.graph.id.clone(), ack: ack.clone() }];
        let mut to_complete = Vec::new();
        let mut to_fail = Vec::new();
        let mut loops = Vec::new();
        for node in &self.graph.nodes {
            if self.states[&node.id] != NodeState::Running {
                continue;
            }
            let Some(task) = node.done_when.ack.as_deref() else { continue };
            if task != ack.task_id {
                continue;
            }
            if node.done_when.is_human_gate() {
                if ack.status != AckStatus::Done {
                    to_fail.push(node.id.clone());
                } else if ack.approved == Some(true) {
                    to_complete.push(node.id.clone());
                } else {
                    let revision = ack.needs_revision.unwrap_or(Revision::Small);
                    let target = Self::loop_target(node, revision);
                    loops.push((node.id.clone(), target, revision));
                }
            } else {
                match ack.status {
                    AckStatus::Done => to_complete.push(node.id.clone()),
                    AckStatus::Failed | AckStatus::Blocked => to_fail.push(node.id.clone()),
                }
            }
        }
        for id in to_fail {
            events.extend(self.fail(&id));
        }
        for id in to_complete {
            events.extend(self.complete(&id));
        }
        for (node, target, revision) in loops {
            events.extend(self.loop_back(&node, &target, revision));
        }
        events
    }

    /// Apply a `done_when.match` pattern: complete running nodes whose pattern
    /// matches the body (test banners etc.).
    #[must_use]
    pub fn apply_match(&mut self, body: &str) -> Vec<WorkflowEvent> {
        let mut to_complete = Vec::new();
        for node in &self.graph.nodes {
            if self.states[&node.id] != NodeState::Running {
                continue;
            }
            let Some(pattern) = node.done_when.r#match.as_deref() else { continue };
            let Ok(re) = regex::Regex::new(pattern) else { continue };
            if re.is_match(body) {
                to_complete.push(node.id.clone());
            }
        }
        let mut events = Vec::new();
        for id in to_complete {
            events.extend(self.complete(&id));
        }
        events
    }

    /// A running node was reported failed (ack failed/blocked, step failed).
    /// Apply the node's `on_error` policy.
    #[must_use]
    pub fn fail(&mut self, id: &str) -> Vec<WorkflowEvent> {
        let Some(node) = self.node(id) else { return Vec::new() };
        if self.states[id] != NodeState::Running {
            return Vec::new();
        }
        match node.on_error {
            OnError::Rerun { max } => {
                let attempts = self.reruns.entry(id.to_owned()).or_insert(0);
                if *attempts < max {
                    *attempts += 1;
                    self.states.insert(id.to_owned(), NodeState::Ready);
                    return vec![WorkflowEvent::NodeReady {
                        graph: self.graph.id.clone(),
                        node: id.to_owned(),
                    }];
                }
                self.states.insert(id.to_owned(), NodeState::Failed);
                vec![WorkflowEvent::NodeFailed {
                    graph: self.graph.id.clone(),
                    node: id.to_owned(),
                }]
            }
            OnError::Skip => {
                self.states.insert(id.to_owned(), NodeState::Done);
                let mut events = vec![WorkflowEvent::NodeDone {
                    graph: self.graph.id.clone(),
                    node: id.to_owned(),
                    skipped: true,
                }];
                events.extend(self.push_ready_events());
                events
            }
            OnError::Delegate => {
                self.states.insert(id.to_owned(), NodeState::NeedsDecision);
                vec![WorkflowEvent::NodeNeedsDecision {
                    graph: self.graph.id.clone(),
                    node: id.to_owned(),
                }]
            }
        }
    }

    /// A running node hit its per-node timeout: completion is ambiguous.
    #[must_use]
    pub fn timeout(&mut self, id: &str) -> Vec<WorkflowEvent> {
        if self.states.get(id) != Some(&NodeState::Running) {
            return Vec::new();
        }
        self.states.insert(id.to_owned(), NodeState::NeedsDecision);
        vec![WorkflowEvent::NodeNeedsDecision { graph: self.graph.id.clone(), node: id.to_owned() }]
    }

    /// The manager's ruling on a `NeedsDecision` node.
    #[must_use]
    pub fn rule(&mut self, id: &str, ruling: ManagerRuling) -> Vec<WorkflowEvent> {
        if self.states.get(id) != Some(&NodeState::NeedsDecision) {
            return Vec::new();
        }
        match ruling {
            ManagerRuling::Done => {
                self.states.insert(id.to_owned(), NodeState::Done);
                let mut events = vec![WorkflowEvent::NodeDone {
                    graph: self.graph.id.clone(),
                    node: id.to_owned(),
                    skipped: false,
                }];
                events.extend(self.push_ready_events());
                events
            }
            ManagerRuling::Skip => {
                self.states.insert(id.to_owned(), NodeState::Done);
                let mut events = vec![WorkflowEvent::NodeDone {
                    graph: self.graph.id.clone(),
                    node: id.to_owned(),
                    skipped: true,
                }];
                events.extend(self.push_ready_events());
                events
            }
            ManagerRuling::Rerun => {
                self.states.insert(id.to_owned(), NodeState::Ready);
                vec![WorkflowEvent::NodeReady { graph: self.graph.id.clone(), node: id.to_owned() }]
            }
            ManagerRuling::Split => {
                self.states.insert(id.to_owned(), NodeState::Blocked);
                vec![WorkflowEvent::NodeBlocked {
                    graph: self.graph.id.clone(),
                    node: id.to_owned(),
                    reason: "split requested by manager".to_owned(),
                }]
            }
        }
    }

    /// The `loop_back` target for a human-gate rejection.
    fn loop_target(node: &NodeDef, revision: Revision) -> String {
        match &node.loop_back {
            Some(lb) => match revision {
                Revision::Big => lb.big.clone(),
                Revision::Small | Revision::Medium | Revision::None => lb.small.clone(),
            },
            // No loop_back configured: re-run the gate node itself.
            None => node.id.clone(),
        }
    }

    /// Loop a human gate back: re-ready `target` and reset everything that
    /// transitively depends on it so the chain re-runs.
    fn loop_back(&mut self, node: &str, target: &str, revision: Revision) -> Vec<WorkflowEvent> {
        let mut events = vec![WorkflowEvent::LoopBack {
            graph: self.graph.id.clone(),
            node: node.to_owned(),
            target: target.to_owned(),
            revision,
        }];
        let downstream = reachable_from(self, target);
        for dep in downstream {
            if dep != target && matches!(self.states[&dep], NodeState::Ready | NodeState::Running) {
                self.states.insert(dep.clone(), NodeState::Pending);
            }
        }
        self.states.insert(target.to_owned(), NodeState::Ready);
        events.push(WorkflowEvent::NodeReady {
            graph: self.graph.id.clone(),
            node: target.to_owned(),
        });
        events
    }

    fn complete(&mut self, id: &str) -> Vec<WorkflowEvent> {
        if self.states[id] != NodeState::Running {
            return Vec::new();
        }
        self.states.insert(id.to_owned(), NodeState::Done);
        let mut events = vec![WorkflowEvent::NodeDone {
            graph: self.graph.id.clone(),
            node: id.to_owned(),
            skipped: false,
        }];
        events.extend(self.push_ready_events());
        events
    }

    fn push_ready_events(&mut self) -> Vec<WorkflowEvent> {
        let mut events = Vec::new();
        loop {
            let newly: Vec<String> = self
                .graph
                .nodes
                .iter()
                .filter(|n| {
                    self.states[&n.id] == NodeState::Pending
                        && n.depends_on.iter().all(|dep| self.states[dep] == NodeState::Done)
                })
                .map(|n| n.id.clone())
                .collect();
            if newly.is_empty() {
                break;
            }
            for id in newly {
                self.states.insert(id.clone(), NodeState::Ready);
                events.push(WorkflowEvent::NodeReady { graph: self.graph.id.clone(), node: id });
            }
        }
        events
    }
}

/// All nodes that transitively depend on `target` (its downstream), target
/// itself excluded.
fn reachable_from(wf: &Workflow, target: &str) -> Vec<String> {
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in &wf.graph.nodes {
        for dep in &node.depends_on {
            dependents.entry(dep.as_str()).or_default().push(&node.id);
        }
    }
    let mut seen = HashSet::new();
    let mut stack = vec![target];
    let mut out = Vec::new();
    while let Some(current) = stack.pop() {
        if let Some(nexts) = dependents.get(current) {
            for next in nexts {
                if seen.insert(*next) {
                    out.push((*next).to_owned());
                    stack.push(next);
                }
            }
        }
    }
    out
}

/// Depth-first search for a dependency cycle; returns the ids along the first
/// cycle found, if any.
fn find_cycle(nodes: &[NodeDef]) -> Option<Vec<String>> {
    const GREY: u8 = 1;
    const BLACK: u8 = 2;

    fn visit<'a>(
        node: &'a NodeDef,
        index: &HashMap<&'a str, &'a NodeDef>,
        color: &mut HashMap<&'a str, u8>,
        stack: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        color.insert(&node.id, GREY);
        stack.push(node.id.clone());
        for dep in &node.depends_on {
            match color.get(dep.as_str()) {
                Some(&GREY) => {
                    let cut = stack.iter().position(|s| s == dep).unwrap_or(0);
                    return Some(stack[cut..].to_vec());
                }
                Some(&BLACK) => {}
                _ => {
                    if let Some(cycle) = visit(index[dep.as_str()], index, color, stack) {
                        return Some(cycle);
                    }
                }
            }
        }
        stack.pop();
        color.insert(&node.id, BLACK);
        None
    }

    let index: HashMap<&str, &NodeDef> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut color: HashMap<&str, u8> = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    for node in nodes {
        if color.get(node.id.as_str()) == Some(&BLACK) {
            continue;
        }
        if let Some(cycle) = visit(node, &index, &mut color, &mut stack) {
            return Some(cycle);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, role: &str, deps: &[&str], task: &str) -> NodeDef {
        NodeDef {
            id: id.to_owned(),
            role: role.to_owned(),
            depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
            start_template: format!("start {id} for {{feature}}"),
            done_when: DoneWhen { ack: Some(task.to_owned()), ..DoneWhen::default() },
            ..NodeDef::default()
        }
    }

    fn chain() -> Workflow {
        Workflow::new(GraphDef {
            id: "chain".to_owned(),
            name: "chain".to_owned(),
            nodes: vec![
                node("design", "designer", &[], "design.done"),
                node("dev", "dev", &["design"], "dev.done"),
            ],
        })
        .unwrap()
    }

    fn roster() -> Vec<RosterEntry> {
        vec![
            RosterEntry {
                agent_id: "designer_01".to_owned(),
                role: "designer".to_owned(),
                state: AgentState::Idle,
                session_id: Some("s1".to_owned()),
                inbox_depth: 0,
            },
            RosterEntry {
                agent_id: "dev_01".to_owned(),
                role: "dev".to_owned(),
                state: AgentState::Idle,
                session_id: Some("s2".to_owned()),
                inbox_depth: 0,
            },
        ]
    }

    fn ack(task: &str) -> Ack {
        Ack {
            task_id: task.to_owned(),
            status: AckStatus::Done,
            summary: None,
            approved: None,
            needs_revision: None,
        }
    }

    #[test]
    fn roots_start_ready_and_downstream_pending() {
        let wf = chain();
        assert_eq!(wf.state("design"), Some(NodeState::Ready));
        assert_eq!(wf.state("dev"), Some(NodeState::Pending));
        assert_eq!(wf.ready().iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), vec!["design"]);
    }

    #[test]
    fn duplicate_ids_rejected() {
        let err = Workflow::new(GraphDef {
            id: "dup".to_owned(),
            name: "dup".to_owned(),
            nodes: vec![node("a", "dev", &[], "a.done"), node("a", "dev", &[], "a.done")],
        });
        assert!(err.is_err());
    }

    #[test]
    fn unknown_dependency_rejected() {
        let err = Workflow::new(GraphDef {
            id: "bad".to_owned(),
            name: "bad".to_owned(),
            nodes: vec![node("a", "dev", &["ghost"], "a.done")],
        });
        assert!(err.is_err());
    }

    #[test]
    fn cycle_rejected() {
        let a = NodeDef { depends_on: vec!["b".to_owned()], ..node("a", "dev", &[], "a.done") };
        let b = NodeDef { depends_on: vec!["a".to_owned()], ..node("b", "dev", &[], "b.done") };
        let err =
            Workflow::new(GraphDef { id: "c".to_owned(), name: "c".to_owned(), nodes: vec![a, b] });
        assert!(err.is_err());
    }

    #[test]
    fn node_without_criterion_is_rejected() {
        let n = NodeDef { done_when: DoneWhen::default(), ..node("a", "dev", &[], "a.done") };
        let err =
            Workflow::new(GraphDef { id: "x".to_owned(), name: "x".to_owned(), nodes: vec![n] });
        assert!(err.is_err());
    }

    #[test]
    fn ack_completes_and_readies_downstream() {
        let mut wf = chain();
        wf.start("design").unwrap();
        let events = wf.apply_ack(&ack("design.done"));
        assert!(events.contains(&WorkflowEvent::NodeDone {
            graph: "chain".to_owned(),
            node: "design".to_owned(),
            skipped: false
        }));
        assert!(events.contains(&WorkflowEvent::NodeReady {
            graph: "chain".to_owned(),
            node: "dev".to_owned()
        }));
        assert_eq!(wf.state("dev"), Some(NodeState::Ready));
        assert!(!wf.is_complete());
    }

    #[test]
    fn ack_for_unknown_task_is_ignored() {
        let mut wf = chain();
        wf.start("design").unwrap();
        assert!(
            !wf.apply_ack(&ack("bogus.done"))
                .iter()
                .any(|e| matches!(e, WorkflowEvent::NodeDone { .. }))
        );
        assert_eq!(wf.state("design"), Some(NodeState::Running));
    }

    #[test]
    fn ack_for_a_node_that_never_started_is_ignored() {
        let mut wf = chain();
        assert!(
            !wf.apply_ack(&ack("design.done"))
                .iter()
                .any(|e| matches!(e, WorkflowEvent::NodeDone { .. }))
        );
        assert_eq!(wf.state("design"), Some(NodeState::Ready));
    }

    #[test]
    fn full_run_completes_the_workflow() {
        let mut wf = chain();
        wf.start("design").unwrap();
        let _ = wf.apply_ack(&ack("design.done"));
        wf.start("dev").unwrap();
        let _ = wf.apply_ack(&ack("dev.done"));
        assert!(wf.is_complete());
    }

    #[test]
    fn render_start_substitutes_variables() {
        let wf = chain();
        let vars = BTreeMap::from([("feature".to_owned(), "auth".to_owned())]);
        assert_eq!(wf.render_start("design", &vars).unwrap(), "start design for auth");
    }

    #[test]
    fn rerun_policy_bounds_attempts() {
        let a = NodeDef {
            on_error: OnError::Rerun { max: 1 },
            ..node("design", "designer", &[], "design.done")
        };
        let mut wf =
            Workflow::new(GraphDef { id: "r".to_owned(), name: "r".to_owned(), nodes: vec![a] })
                .unwrap();
        wf.start("design").unwrap();
        let first = wf.fail("design");
        assert!(first.contains(&WorkflowEvent::NodeReady {
            graph: "r".to_owned(),
            node: "design".to_owned()
        }));
        assert_eq!(wf.reruns("design"), 1);
        wf.start("design").unwrap();
        let second = wf.fail("design");
        assert!(second.contains(&WorkflowEvent::NodeFailed {
            graph: "r".to_owned(),
            node: "design".to_owned()
        }));
        assert_eq!(wf.state("design"), Some(NodeState::Failed));
    }

    #[test]
    fn skip_policy_marks_done_and_readies_downstream() {
        let a =
            NodeDef { on_error: OnError::Skip, ..node("design", "designer", &[], "design.done") };
        let mut wf = Workflow::new(GraphDef {
            id: "s".to_owned(),
            name: "s".to_owned(),
            nodes: vec![a, node("dev", "dev", &["design"], "dev.done")],
        })
        .unwrap();
        wf.start("design").unwrap();
        let events = wf.fail("design");
        assert!(events.contains(&WorkflowEvent::NodeDone {
            graph: "s".to_owned(),
            node: "design".to_owned(),
            skipped: true
        }));
        assert!(
            events.contains(&WorkflowEvent::NodeReady {
                graph: "s".to_owned(),
                node: "dev".to_owned()
            })
        );
        assert_eq!(wf.state("dev"), Some(NodeState::Ready));
    }

    #[test]
    fn delegate_policy_moves_to_needs_decision() {
        let mut wf = chain();
        wf.start("design").unwrap();
        assert!(wf.fail("design").contains(&WorkflowEvent::NodeNeedsDecision {
            graph: "chain".to_owned(),
            node: "design".to_owned()
        }));
        assert_eq!(wf.state("design"), Some(NodeState::NeedsDecision));
    }

    #[test]
    fn manager_ruling_completes() {
        let a = NodeDef {
            on_error: OnError::Delegate,
            ..node("design", "designer", &[], "design.done")
        };
        let mut wf =
            Workflow::new(GraphDef { id: "g".to_owned(), name: "g".to_owned(), nodes: vec![a] })
                .unwrap();
        wf.start("design").unwrap();
        let _ = wf.fail("design");
        assert!(
            wf.rule("design", ManagerRuling::Done)
                .iter()
                .any(|e| matches!(e, WorkflowEvent::NodeDone { .. }))
        );
        assert_eq!(wf.state("design"), Some(NodeState::Done));
        assert!(wf.is_complete());
    }

    #[test]
    fn manager_rerun_re_readies_the_node() {
        let mut wf = chain();
        wf.start("design").unwrap();
        let _ = wf.fail("design");
        assert!(wf.rule("design", ManagerRuling::Rerun).contains(&WorkflowEvent::NodeReady {
            graph: "chain".to_owned(),
            node: "design".to_owned()
        }));
    }

    #[test]
    fn timeout_moves_running_to_needs_decision() {
        let mut wf = chain();
        wf.start("design").unwrap();
        assert!(wf.timeout("design").contains(&WorkflowEvent::NodeNeedsDecision {
            graph: "chain".to_owned(),
            node: "design".to_owned()
        }));
        assert_eq!(wf.state("design"), Some(NodeState::NeedsDecision));
        assert!(wf.timeout("dev").is_empty(), "a non-running node cannot time out");
    }

    #[test]
    fn done_when_match_completes_on_output_pattern() {
        let t = NodeDef {
            done_when: DoneWhen { r#match: Some("ALL GREEN".to_owned()), ..DoneWhen::default() },
            ..node("test", "tester", &[], "unused")
        };
        let mut wf =
            Workflow::new(GraphDef { id: "m".to_owned(), name: "m".to_owned(), nodes: vec![t] })
                .unwrap();
        wf.start("test").unwrap();
        assert!(
            wf.apply_match("Ran 42 tests. ALL GREEN")
                .iter()
                .any(|e| matches!(e, WorkflowEvent::NodeDone { .. }))
        );
    }

    #[test]
    fn human_gate_approves_only_on_approved_ack() {
        let gate = NodeDef {
            done_when: DoneWhen {
                ack: Some("gate".to_owned()),
                approved: Some(true),
                ..DoneWhen::default()
            },
            loop_back: Some(LoopBack {
                on: "needs_revision".to_owned(),
                small: "gate".to_owned(),
                big: "review".to_owned(),
            }),
            ..node("gate", "designer", &["review"], "gate")
        };
        let mut wf = Workflow::new(GraphDef {
            id: "g".to_owned(),
            name: "g".to_owned(),
            nodes: vec![node("review", "reviewer", &[], "review.done"), gate],
        })
        .unwrap();
        assert_eq!(wf.state("review"), Some(NodeState::Ready));
        wf.start("review").unwrap();
        let _ = wf.apply_ack(&ack("review.done"));
        assert_eq!(wf.state("gate"), Some(NodeState::Ready));
        wf.start("gate").unwrap();

        // approved:false + small → loop back to the gate itself.
        let reject = Ack {
            task_id: "gate".to_owned(),
            status: AckStatus::Done,
            summary: Some("tweak the wording".to_owned()),
            approved: Some(false),
            needs_revision: Some(Revision::Small),
        };
        let events = wf.apply_ack(&reject);
        assert!(events.contains(&WorkflowEvent::LoopBack {
            graph: "g".to_owned(),
            node: "gate".to_owned(),
            target: "gate".to_owned(),
            revision: Revision::Small
        }));
        assert_eq!(wf.state("gate"), Some(NodeState::Ready), "small revision re-readies the gate");

        // approved:true → done.
        wf.start("gate").unwrap();
        let approve = Ack {
            task_id: "gate".to_owned(),
            status: AckStatus::Done,
            summary: None,
            approved: Some(true),
            needs_revision: None,
        };
        let events = wf.apply_ack(&approve);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkflowEvent::NodeDone { node, .. } if node == "gate"))
        );
        assert_eq!(wf.state("gate"), Some(NodeState::Done));
    }

    #[test]
    fn human_gate_big_revision_reruns_the_review_chain() {
        let review = node("review", "reviewer", &[], "review.done");
        let gate = NodeDef {
            done_when: DoneWhen {
                ack: Some("gate".to_owned()),
                approved: Some(true),
                ..DoneWhen::default()
            },
            loop_back: Some(LoopBack {
                on: "needs_revision".to_owned(),
                small: "gate".to_owned(),
                big: "review".to_owned(),
            }),
            ..node("gate", "designer", &["review"], "gate")
        };
        let mut wf = Workflow::new(GraphDef {
            id: "b".to_owned(),
            name: "b".to_owned(),
            nodes: vec![review, gate],
        })
        .unwrap();
        wf.start("review").unwrap();
        let _ = wf.apply_ack(&ack("review.done"));
        wf.start("gate").unwrap();
        let reject = Ack {
            task_id: "gate".to_owned(),
            status: AckStatus::Done,
            summary: None,
            approved: Some(false),
            needs_revision: Some(Revision::Big),
        };
        let events = wf.apply_ack(&reject);
        assert!(events.contains(&WorkflowEvent::LoopBack {
            graph: "b".to_owned(),
            node: "gate".to_owned(),
            target: "review".to_owned(),
            revision: Revision::Big
        }));
        assert_eq!(
            wf.state("review"),
            Some(NodeState::Ready),
            "big revision re-readies the review"
        );
        assert_eq!(wf.state("gate"), Some(NodeState::Pending), "gate waits on the re-run review");
    }

    #[test]
    fn failed_ack_triggers_on_error() {
        let a =
            NodeDef { on_error: OnError::Rerun { max: 1 }, ..node("dev", "dev", &[], "dev.done") };
        let mut wf =
            Workflow::new(GraphDef { id: "f".to_owned(), name: "f".to_owned(), nodes: vec![a] })
                .unwrap();
        wf.start("dev").unwrap();
        let failed = Ack {
            task_id: "dev.done".to_owned(),
            status: AckStatus::Failed,
            summary: None,
            approved: None,
            needs_revision: None,
        };
        assert!(
            wf.apply_ack(&failed).contains(&WorkflowEvent::NodeReady {
                graph: "f".to_owned(),
                node: "dev".to_owned()
            })
        );
    }

    #[test]
    fn role_resolution_uses_explicit_agent_first() {
        let explicit =
            NodeDef { agent_id: Some("dev_09".to_owned()), ..node("dev", "dev", &[], "dev.done") };
        let wf = Workflow::new(GraphDef {
            id: "e".to_owned(),
            name: "e".to_owned(),
            nodes: vec![explicit],
        })
        .unwrap();
        assert_eq!(
            wf.resolve_target("dev", &roster()),
            RoleResolution::Target("dev_09".to_owned())
        );
    }

    #[test]
    fn role_resolution_prefers_least_loaded_idle_agent() {
        let roster = vec![
            RosterEntry {
                agent_id: "designer_01".to_owned(),
                role: "designer".to_owned(),
                state: AgentState::Idle,
                session_id: Some("s1".to_owned()),
                inbox_depth: 1,
            },
            RosterEntry {
                agent_id: "designer_02".to_owned(),
                role: "designer".to_owned(),
                state: AgentState::Idle,
                session_id: Some("s3".to_owned()),
                inbox_depth: 0,
            },
        ];
        let wf = chain();
        assert_eq!(
            wf.resolve_target("design", &roster),
            RoleResolution::Target("designer_02".to_owned()),
            "the least-loaded idle designer wins"
        );
    }

    #[test]
    fn role_resolution_falls_back_to_first_matching_agent_when_none_idle() {
        let mut roster = roster();
        roster[0].state = AgentState::Working;
        roster[1].state = AgentState::Working;
        let wf = chain();
        assert_eq!(
            wf.resolve_target("design", &roster),
            RoleResolution::Target("designer_01".to_owned())
        );
    }

    #[test]
    fn role_resolution_reports_missing_role() {
        let wf = chain();
        let no_designer = vec![RosterEntry {
            agent_id: "dev_01".to_owned(),
            role: "dev".to_owned(),
            state: AgentState::Idle,
            session_id: None,
            inbox_depth: 0,
        }];
        assert_eq!(
            wf.resolve_target("design", &no_designer),
            RoleResolution::MissingRole { role: "designer".to_owned() }
        );
        assert_eq!(
            wf.missing_roles(&no_designer),
            vec![("design".to_owned(), "designer".to_owned())]
        );
    }

    #[test]
    fn parse_graph_json_spec_shape() {
        let json = r#"
        {
          "id": "mini",
          "name": "mini",
          "nodes": [
            { "id": "brainstorm", "role": "designer", "depends_on": [],
              "start_template": "Research {feature}.", "done_when": { "ack": "brainstorm" } },
            { "id": "dev", "role": "dev", "depends_on": ["brainstorm"],
              "start_template": "Implement {feature}.", "done_when": { "ack": "dev" },
              "on_error": { "rerun": { "max": 2 } }, "mode": "background" }
          ]
        }
        "#;
        let wf = Workflow::parse_json(json).unwrap();
        assert_eq!(wf.graph.id, "mini");
        assert_eq!(wf.state("brainstorm"), Some(NodeState::Ready));
        let dev = wf.node("dev").unwrap();
        assert_eq!(dev.on_error.max_reruns(), 2);
        assert_eq!(dev.mode, AgentMode::Background);
    }

    #[test]
    fn attempts_track_starts() {
        let mut wf = chain();
        assert_eq!(wf.attempts("design"), 0);
        wf.start("design").unwrap();
        assert_eq!(wf.attempts("design"), 1);
        assert_eq!(wf.start("design"), None, "a running node cannot start twice");
        assert_eq!(wf.attempts("design"), 1);
    }

    #[test]
    fn split_ruling_blocks_the_node() {
        let mut wf = chain();
        wf.start("design").unwrap();
        let _ = wf.fail("design");
        assert!(wf.rule("design", ManagerRuling::Split).contains(&WorkflowEvent::NodeBlocked {
            graph: "chain".to_owned(),
            node: "design".to_owned(),
            reason: "split requested by manager".to_owned()
        }));
        assert_eq!(wf.state("design"), Some(NodeState::Blocked));
    }

    #[test]
    fn manager_ruling_parses_actions() {
        assert_eq!("done".parse(), Ok(ManagerRuling::Done));
        assert_eq!("rerun".parse(), Ok(ManagerRuling::Rerun));
        assert_eq!("skip".parse(), Ok(ManagerRuling::Skip));
        assert_eq!("split".parse(), Ok(ManagerRuling::Split));
        assert_eq!("explode".parse::<ManagerRuling>(), Err(()));
    }

    #[test]
    fn states_report_in_definition_order() {
        let wf = chain();
        assert_eq!(wf.states().len(), 2);
        assert_eq!(wf.states()[0], ("design", NodeState::Ready));
        assert_eq!(wf.states()[1], ("dev", NodeState::Pending));
    }

    #[test]
    fn restore_states_remaps_running_to_ready_and_keeps_terminals() {
        let mut wf = chain();
        wf.restore_states(vec![
            ("design".to_owned(), NodeState::Running),
            ("dev".to_owned(), NodeState::Done),
        ]);
        assert_eq!(
            wf.state("design"),
            Some(NodeState::Ready),
            "M3: a running node becomes ready so its start re-delivers"
        );
        assert_eq!(wf.state("dev"), Some(NodeState::Done), "terminal states are kept");
    }
}
