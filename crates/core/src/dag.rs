//! Workflows: a DAG of tasks across agents.
//!
//! A [`Workflow`] is a declarative graph (see
//! `docs/specs/2026-08-10-orchestration.md`). Nodes name the role that owns the
//! task and a [`DoneWhen`] criterion; edges are `depends_on`. The engine is
//! pure and offline: [`Workflow::apply_ack`] marks a running node done and
//! reports the newly-ready nodes, and the orchestrator turns each `Ready` event
//! into a posted start message. No LLM in the happy path — ambiguity is a
//! distinct node state (`NeedsDecision`) the orchestrator escalates.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// The lifecycle of one workflow node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Not ready yet: a dependency is still unfinished.
    #[default]
    Pending,
    /// Dependencies are done; the orchestrator may start it.
    Ready,
    /// A start message has been posted; the owning agent is working on it.
    Running,
    /// Blocked on something outside the DAG (e.g. a dependency agent that is
    /// itself blocked). Escalation-worthy, but not failed.
    Blocked,
    /// The node's `done_when` fired.
    Done,
    /// The node errored past its rerun bound.
    Failed,
    /// Completion is ambiguous (timeout, no ack, conflicting signals). The
    /// manager must rule.
    NeedsDecision,
}

/// How a node proves it finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DoneWhen {
    /// The agent posted an ack with this task id (e.g. `"dev.done"`).
    Ack { task: String },
    /// A status/event message whose body contains this marker arrived.
    Status { contains: String },
}

/// What the orchestrator does when a running node is reported failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on_error", rename_all = "snake_case")]
pub enum OnError {
    /// Re-post the start message, up to `max` extra attempts. Bounded in code
    /// so an agent cannot spin forever.
    Rerun { max: u8 },
    /// Mark done anyway (e.g. cosmetic nodes).
    Skip,
    /// Hand the ruling to the manager.
    Delegate,
}

impl Default for OnError {
    fn default() -> Self {
        Self::Rerun { max: 1 }
    }
}

/// One task in a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDef {
    pub id: String,
    /// The role that owns this task; the orchestrator resolves it to an agent.
    pub role: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The instruction to post when the node starts. `{key}` placeholders
    /// render workflow variables (feature, spec, refs).
    pub start: String,
    pub done_when: DoneWhen,
    #[serde(default)]
    pub on_error: OnError,
}

/// A node lifecycle event, for the orchestrator to post to the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum NodeEvent {
    /// All dependencies done; a start message should be posted.
    Ready { node: String },
    /// The orchestrator posted the start message.
    Started { node: String },
    /// The node completed.
    Done { node: String, skipped: bool },
    /// The node failed past its rerun bound.
    Failed { node: String },
    /// Completion is ambiguous; the manager must rule.
    NeedsDecision { node: String },
}

/// A running workflow instance. State lives here and only here; a fresh
/// instance of the same definition starts from `Pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    nodes: Vec<NodeDef>,
    states: BTreeMap<String, NodeState>,
    reruns: BTreeMap<String, u8>,
}

impl Workflow {
    /// Validate and build a workflow.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidWorkflow`] for duplicate ids, a dependency
    /// on an unknown node, or a dependency cycle — a workflow that can never
    /// finish is rejected at load, not discovered at runtime.
    pub fn new(name: impl Into<String>, nodes: Vec<NodeDef>) -> Result<Self, CoreError> {
        let name = name.into();
        let err = |reason: String| CoreError::InvalidWorkflow { name: name.clone(), reason };

        let mut ids = HashSet::new();
        for node in &nodes {
            if node.id.is_empty() {
                return Err(err("a node id must not be empty".to_owned()));
            }
            if !ids.insert(node.id.clone()) {
                return Err(err(format!("duplicate node id {:?}", node.id)));
            }
        }
        for node in &nodes {
            for dep in &node.depends_on {
                if !ids.contains(dep) {
                    return Err(err(format!(
                        "node {:?} depends on unknown node {:?}",
                        node.id, dep
                    )));
                }
            }
        }
        if let Some(cycle) = find_cycle(&nodes) {
            return Err(err(format!("dependency cycle: {cycle:?}")));
        }

        let states = nodes
            .iter()
            .map(|n| {
                (
                    n.id.clone(),
                    if n.depends_on.is_empty() { NodeState::Ready } else { NodeState::Pending },
                )
            })
            .collect();
        Ok(Self { name, nodes, states, reruns: BTreeMap::new() })
    }

    /// Parse a TOML document of `[[node]]` entries into a named workflow.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidWorkflow`] for schema or graph problems.
    pub fn parse_toml(name: &str, input: &str) -> Result<Self, CoreError> {
        #[derive(Deserialize)]
        struct WorkflowFile {
            #[serde(default)]
            node: Vec<NodeDef>,
        }
        let file: WorkflowFile = toml::from_str(input).map_err(|e| CoreError::InvalidWorkflow {
            name: name.to_owned(),
            reason: format!("invalid workflow TOML: {e}"),
        })?;
        Self::new(name, file.node)
    }

    #[must_use]
    pub fn node(&self, id: &str) -> Option<&NodeDef> {
        self.nodes.iter().find(|n| n.id == id)
    }

    #[must_use]
    pub fn nodes(&self) -> &[NodeDef] {
        &self.nodes
    }

    #[must_use]
    pub fn state(&self, id: &str) -> Option<NodeState> {
        self.states.get(id).copied()
    }

    /// The states of every node, in definition order, for `dag status`.
    #[must_use]
    pub fn states(&self) -> Vec<(&str, NodeState)> {
        self.nodes
            .iter()
            .filter_map(|n| self.states.get(&n.id).map(|s| (n.id.as_str(), *s)))
            .collect()
    }

    /// True when every node is `Done` (including skipped).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.nodes.iter().all(|n| self.states[&n.id] == NodeState::Done)
    }

    /// The nodes currently in `Ready`, in definition order. After an
    /// orchestrator restart this is the resume list: start every node here.
    #[must_use]
    pub fn ready(&self) -> Vec<&NodeDef> {
        self.nodes.iter().filter(|n| self.states[&n.id] == NodeState::Ready).collect()
    }

    /// Mark a ready node as started (the orchestrator posts its start message
    /// at the same time).
    #[must_use]
    pub fn start(&mut self, id: &str) -> Option<NodeEvent> {
        if self.states.get(id)? != &NodeState::Ready {
            return None;
        }
        self.states.insert(id.to_owned(), NodeState::Running);
        Some(NodeEvent::Started { node: id.to_owned() })
    }

    /// Render a node's start instruction against workflow variables.
    #[must_use]
    pub fn render_start(&self, id: &str, vars: &BTreeMap<String, String>) -> Option<String> {
        let node = self.node(id)?;
        let mut rendered = node.start.clone();
        for (key, value) in vars {
            rendered = rendered.replace(&format!("{{{key}}}"), value);
        }
        Some(rendered)
    }

    /// Apply an ack: complete every *running* node whose `done_when` matches,
    /// then report newly-ready nodes. A node that was never started cannot be
    /// completed by an ack.
    #[must_use]
    pub fn apply_ack(&mut self, task: &str) -> Vec<NodeEvent> {
        let mut events = Vec::new();
        for node in &self.nodes {
            if matches!(node.done_when, DoneWhen::Ack { task: ref t } if t == task)
                && self.states[&node.id] == NodeState::Running
            {
                self.states.insert(node.id.clone(), NodeState::Done);
                events.push(NodeEvent::Done { node: node.id.clone(), skipped: false });
            }
        }
        self.push_ready_events(&mut events);
        events
    }

    /// Apply a status marker: like [`Workflow::apply_ack`] but for
    /// [`DoneWhen::Status`].
    #[must_use]
    pub fn apply_status(&mut self, body: &str) -> Vec<NodeEvent> {
        let mut events = Vec::new();
        for node in &self.nodes {
            if matches!(node.done_when, DoneWhen::Status { contains: ref c } if body.contains(c.as_str()))
                && self.states[&node.id] == NodeState::Running
            {
                self.states.insert(node.id.clone(), NodeState::Done);
                events.push(NodeEvent::Done { node: node.id.clone(), skipped: false });
            }
        }
        self.push_ready_events(&mut events);
        events
    }

    /// A running node was reported failed. Apply the node's `on_error` policy:
    /// rerun within bounds, skip, or defer to the manager. Returns the events
    /// the orchestrator should act on, including any downstream nodes a skip
    /// makes ready.
    #[must_use]
    pub fn fail(&mut self, id: &str) -> Option<Vec<NodeEvent>> {
        if self.states.get(id)? != &NodeState::Running {
            return None;
        }
        let on_error = self.node(id)?.on_error.clone();
        match on_error {
            OnError::Rerun { max } => {
                let attempts = self.reruns.entry(id.to_owned()).or_insert(0);
                if *attempts < max {
                    *attempts += 1;
                    self.states.insert(id.to_owned(), NodeState::Ready);
                    return Some(vec![NodeEvent::Ready { node: id.to_owned() }]);
                }
                self.states.insert(id.to_owned(), NodeState::Failed);
                Some(vec![NodeEvent::Failed { node: id.to_owned() }])
            }
            OnError::Skip => {
                self.states.insert(id.to_owned(), NodeState::Done);
                let mut events = vec![NodeEvent::Done { node: id.to_owned(), skipped: true }];
                self.push_ready_events(&mut events);
                Some(events)
            }
            OnError::Delegate => {
                self.states.insert(id.to_owned(), NodeState::NeedsDecision);
                Some(vec![NodeEvent::NeedsDecision { node: id.to_owned() }])
            }
        }
    }

    /// The manager's ruling on a `NeedsDecision` node: finish it (skipped
    /// counts as done) or fail it. Returns the events to act on, including any
    /// downstream nodes the ruling makes ready.
    #[must_use]
    pub fn rule(&mut self, id: &str, decision: Decision) -> Option<Vec<NodeEvent>> {
        if self.states.get(id)? != &NodeState::NeedsDecision {
            return None;
        }
        match decision {
            Decision::Done => {
                self.states.insert(id.to_owned(), NodeState::Done);
                let mut events = vec![NodeEvent::Done { node: id.to_owned(), skipped: false }];
                self.push_ready_events(&mut events);
                Some(events)
            }
            Decision::Fail => {
                self.states.insert(id.to_owned(), NodeState::Failed);
                Some(vec![NodeEvent::Failed { node: id.to_owned() }])
            }
        }
    }

    /// Rerun attempts so far for a node, for dashboards and bake-back.
    #[must_use]
    pub fn reruns(&self, id: &str) -> u8 {
        self.reruns.get(id).copied().unwrap_or(0)
    }

    fn push_ready_events(&mut self, events: &mut Vec<NodeEvent>) {
        let newly: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| {
                self.states[&n.id] == NodeState::Pending
                    && n.depends_on.iter().all(|dep| self.states[dep] == NodeState::Done)
            })
            .map(|n| n.id.clone())
            .collect();
        for id in newly {
            self.states.insert(id.clone(), NodeState::Ready);
            events.push(NodeEvent::Ready { node: id });
        }
    }
}

/// A manager ruling on a `NeedsDecision` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Done,
    Fail,
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
            start: format!("start {id} for {{feature}}"),
            done_when: DoneWhen::Ack { task: task.to_owned() },
            on_error: OnError::Rerun { max: 1 },
        }
    }

    fn chain() -> Workflow {
        Workflow::new(
            "chain",
            vec![
                node("design", "designer", &[], "design.done"),
                node("dev", "dev", &["design"], "dev.done"),
            ],
        )
        .unwrap()
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
        let err = Workflow::new(
            "dup",
            vec![node("a", "dev", &[], "a.done"), node("a", "dev", &[], "a.done")],
        );
        assert!(err.is_err());
    }

    #[test]
    fn unknown_dependency_rejected() {
        let err = Workflow::new("bad", vec![node("a", "dev", &["ghost"], "a.done")]);
        assert!(err.is_err());
    }

    #[test]
    fn cycle_rejected() {
        let a = NodeDef { depends_on: vec!["b".to_owned()], ..node("a", "dev", &[], "a.done") };
        let b = NodeDef { depends_on: vec!["a".to_owned()], ..node("b", "dev", &[], "b.done") };
        let err = Workflow::new("cycle", vec![a, b]);
        assert!(err.is_err());
    }

    #[test]
    fn ack_completes_and_readies_downstream() {
        let mut wf = chain();
        wf.start("design").unwrap();
        assert_eq!(wf.state("design"), Some(NodeState::Running));
        let events = wf.apply_ack("design.done");
        assert!(events.contains(&NodeEvent::Done { node: "design".to_owned(), skipped: false }));
        assert!(events.contains(&NodeEvent::Ready { node: "dev".to_owned() }));
        assert_eq!(wf.state("dev"), Some(NodeState::Ready));
        assert!(!wf.is_complete());
    }

    #[test]
    fn ack_for_unknown_task_is_ignored() {
        let mut wf = chain();
        wf.start("design").unwrap();
        assert!(wf.apply_ack("bogus.done").is_empty());
        assert_eq!(wf.state("design"), Some(NodeState::Running));
    }

    #[test]
    fn ack_for_a_node_that_never_started_is_ignored() {
        let mut wf = chain();
        let events = wf.apply_ack("design.done");
        assert!(!events.contains(&NodeEvent::Done { node: "design".to_owned(), skipped: false }));
        assert_eq!(wf.state("design"), Some(NodeState::Ready));
    }

    #[test]
    fn full_run_completes_the_workflow() {
        let mut wf = chain();
        wf.start("design").unwrap();
        let _ = wf.apply_ack("design.done");
        wf.start("dev").unwrap();
        let _ = wf.apply_ack("dev.done");
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
        let mut wf = chain();
        wf.start("design").unwrap();
        let first = wf.fail("design").unwrap();
        assert_eq!(first, vec![NodeEvent::Ready { node: "design".to_owned() }]);
        assert_eq!(wf.state("design"), Some(NodeState::Ready));
        assert_eq!(wf.reruns("design"), 1);
        wf.start("design").unwrap();
        let second = wf.fail("design").unwrap();
        assert_eq!(second, vec![NodeEvent::Failed { node: "design".to_owned() }]);
        assert_eq!(wf.state("design"), Some(NodeState::Failed));
    }

    #[test]
    fn skip_policy_marks_done_and_readies_downstream() {
        let a =
            NodeDef { on_error: OnError::Skip, ..node("design", "designer", &[], "design.done") };
        let mut wf =
            Workflow::new("skip", vec![a, node("dev", "dev", &["design"], "dev.done")]).unwrap();
        wf.start("design").unwrap();
        let events = wf.fail("design").unwrap();
        assert_eq!(
            events,
            vec![
                NodeEvent::Done { node: "design".to_owned(), skipped: true },
                NodeEvent::Ready { node: "dev".to_owned() },
            ]
        );
        assert_eq!(wf.state("dev"), Some(NodeState::Ready));
    }

    #[test]
    fn delegate_policy_moves_to_needs_decision() {
        let a = NodeDef {
            on_error: OnError::Delegate,
            ..node("design", "designer", &[], "design.done")
        };
        let mut wf = Workflow::new("delegate", vec![a]).unwrap();
        wf.start("design").unwrap();
        assert_eq!(
            wf.fail("design").unwrap(),
            vec![NodeEvent::NeedsDecision { node: "design".to_owned() }]
        );
        assert_eq!(wf.state("design"), Some(NodeState::NeedsDecision));
    }

    #[test]
    fn manager_ruling_completes_or_fails() {
        let a = NodeDef {
            on_error: OnError::Delegate,
            ..node("design", "designer", &[], "design.done")
        };
        let mut wf = Workflow::new("ruling", vec![a]).unwrap();
        wf.start("design").unwrap();
        let _ = wf.fail("design");
        let events = wf.rule("design", Decision::Done).unwrap();
        assert_eq!(events, vec![NodeEvent::Done { node: "design".to_owned(), skipped: false }]);
        assert!(wf.is_complete());
    }

    #[test]
    fn parse_toml_spec_shape() {
        let input = r#"
[[node]]
id = "dev"
role = "dev"
start = "Implement {feature} from the design; ack dev.done"
done_when = { kind = "ack", task = "dev.done" }
"#;
        let wf = Workflow::parse_toml("t", input).unwrap();
        assert_eq!(wf.node("dev").unwrap().role, "dev");
        assert_eq!(wf.state("dev"), Some(NodeState::Ready), "a root node starts ready");
    }

    #[test]
    fn status_criterion_fires_on_body_marker() {
        let mut wf = Workflow::new(
            "status",
            vec![NodeDef {
                done_when: DoneWhen::Status { contains: "ALL GREEN".to_owned() },
                ..node("test", "tester", &[], "ignored")
            }],
        )
        .unwrap();
        wf.start("test").unwrap();
        let events = wf.apply_status("Ran 42 tests. ALL GREEN");
        assert!(events.contains(&NodeEvent::Done { node: "test".to_owned(), skipped: false }));
    }

    #[test]
    fn ready_never_reports_running_nodes() {
        let mut wf = chain();
        wf.start("design").unwrap();
        let ready: Vec<&str> = wf.ready().iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ready, Vec::<&str>::new(), "a running node is not ready");
        assert_eq!(wf.state("dev"), Some(NodeState::Pending));
    }

    #[test]
    fn states_report_in_definition_order() {
        let wf = chain();
        let states = wf.states();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0], ("design", NodeState::Ready));
        assert_eq!(states[1], ("dev", NodeState::Pending));
    }
}
