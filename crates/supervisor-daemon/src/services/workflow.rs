//! The workflow engine runner (C10): drives [`Workflow`] instances per
//! `(workspace, graph)` over the fleet, resolving roles to agents, delivering
//! start messages through the inbox, and resolving ACKs with the layered
//! resolver (§4.9) on idle.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use supervisor_core::ack::resolve_ack;
use supervisor_core::dag::{ManagerRuling, RoleResolution, RosterEntry, Workflow, WorkflowEvent};
use supervisor_core::event::{BusEvent, FleetEvent, HumanEvent, InboxEvent};
use supervisor_core::signal::Signal;
use supervisor_core::types::{InboxEntry, NodeState, Priority, WorkspaceState};
use supervisor_core::{NodeStateRow, now_rfc3339};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::bus::Receiver;
use crate::clients::registry::DriverRegistry;
use crate::services::workspace::WorkspaceManager;
use crate::state::Fleet;

/// How often the timeout sweep runs.
const TIMEOUT_SWEEP: Duration = Duration::from_secs(5);

/// A running task: the `(graph, node)` an agent is working on.
pub type RunningTask = (String, String);

/// A `(ws, graph_id, node)` key.
pub type TaskKey = (String, String, String);

/// The workflow engine runner.
pub struct WorkflowRunner {
    fleet: Arc<AsyncMutex<Fleet>>,
    drivers: Arc<DriverRegistry>,
    /// F4: the command dispatcher routes `start`/`rule` here and brings
    /// workspaces on demand.
    workspaces: Arc<WorkspaceManager>,
    bus: crate::bus::SharedBus,
    shutdown: CancellationToken,
    /// `(ws, graph_id)` → running instance.
    instances: Mutex<HashMap<(String, String), Workflow>>,
    /// `(ws, graph_id, node)` → render variables.
    vars: Mutex<HashMap<TaskKey, BTreeMap<String, String>>>,
    /// `(ws, agent)` → the tasks the agent is responsible for (a queue, so a
    /// second concurrent workflow cannot overwrite the first and strand it
    /// Running until timeout — review finding 4).
    running: Mutex<HashMap<(String, String), Vec<RunningTask>>>,
    /// `(ws, graph, node)` → started-at deadline.
    deadlines: Mutex<HashMap<TaskKey, Instant>>,
}

impl WorkflowRunner {
    /// Build the runner.
    #[must_use]
    pub fn new(
        fleet: Arc<AsyncMutex<Fleet>>,
        drivers: Arc<DriverRegistry>,
        workspaces: Arc<WorkspaceManager>,
        bus: crate::bus::SharedBus,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            fleet,
            drivers,
            workspaces,
            bus,
            shutdown,
            instances: Mutex::new(HashMap::new()),
            vars: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            deadlines: Mutex::new(HashMap::new()),
        }
    }

    /// Run the main loop + the timeout sweep until shutdown. Restores
    /// previously-started workflows first (M3).
    pub async fn run(&self) {
        self.restore().await;
        let mut rx: Receiver = self.bus.subscribe();
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                () = tokio::time::sleep(TIMEOUT_SWEEP) => self.sweep_timeouts().await,
                event = rx.recv_or_shutdown() => {
                    match event {
                        Some(BusEvent::Human(HumanEvent::Command { command, args })) => {
                            self.on_command(&command, &args).await;
                        }
                        Some(event) => self.handle(event).await,
                        None => return,
                    }
                }
            }
        }
    }

    /// M3: rebuild in-memory instances for every journaled `workflow.start`
    /// after a daemon restart. Running nodes become `Ready` (start messages
    /// re-deliver; task-id idempotency absorbs duplicates); the `running`/
    /// `deadlines` maps are deliberately not restored — a restarted daemon
    /// cannot know which turn belongs to which node.
    async fn restore(&self) {
        let starts = {
            let fleet = self.fleet.lock().await;
            fleet
                .workflow_starts()
                .map(|(ws, graph, vars)| (ws.to_owned(), graph.to_owned(), vars.clone()))
                .collect::<Vec<_>>()
        };
        for (ws, graph, vars) in starts {
            let Ok(mut instance) = self.load_instance(&graph).await else {
                tracing::warn!(ws = %ws, graph = %graph, "restore: unknown graph, skipping");
                continue;
            };
            let states = {
                let fleet = self.fleet.lock().await;
                fleet.node_states(&graph).map(|r| (r.node_id.clone(), r.state)).collect::<Vec<_>>()
            };
            instance.restore_states(states);
            {
                let mut instances =
                    self.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                instances.insert((ws.clone(), graph.clone()), instance.clone());
            }
            for node in instance.nodes() {
                self.vars
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert((ws.clone(), graph.clone(), node.id.clone()), vars.clone());
            }
            tracing::info!(ws = %ws, graph = %graph, "restored workflow after restart");
            for node in instance.ready() {
                self.handle_event(
                    &ws,
                    WorkflowEvent::NodeReady { graph: graph.clone(), node: node.id.clone() },
                )
                .await;
            }
        }
    }

    /// F4: the sole consumer of workflow-related commands.
    ///
    /// - `start` → `args = [ws, graph, vars_json?]`
    /// - `rule`  → `args = [ws, graph, node, action, to?, body?]`
    pub async fn on_command(&self, command: &str, args: &[String]) {
        match command {
            "start" => {
                let (Some(ws), Some(graph)) = (args.first(), args.get(1)) else {
                    tracing::warn!("start command needs ws + graph");
                    return;
                };
                let vars = args
                    .get(2)
                    .and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(s).ok())
                    .unwrap_or_default();
                if let Err(e) = self.workspaces.on(ws).await {
                    tracing::error!(ws = %ws, error = %e, "start: workspace on failed");
                    return;
                }
                if let Err(e) = self.start_graph(ws, graph, vars).await {
                    tracing::error!(ws = %ws, graph = %graph, error = %e, "start graph failed");
                }
            }
            "rule" => {
                let (Some(ws), Some(graph), Some(node), Some(action)) =
                    (args.first(), args.get(1), args.get(2), args.get(3))
                else {
                    tracing::warn!("rule command needs ws + graph + node + action");
                    return;
                };
                let Ok(ruling) = action.parse::<ManagerRuling>() else {
                    tracing::warn!(action = %action, "unknown manager ruling");
                    return;
                };
                let events = {
                    let mut instances =
                        self.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(instance) = instances.get_mut(&(ws.clone(), graph.clone())) else {
                        tracing::warn!(ws = %ws, graph = %graph, "rule: no instance for graph");
                        return;
                    };
                    instance.rule(node, ruling)
                };
                for event in events {
                    self.handle_event(ws, event).await;
                }
            }
            other => tracing::debug!(command = other, "unhandled command"),
        }
    }

    /// The `(graph, node)` an agent is currently working on, if any (F4 — node
    /// context for the rule engine's `Situation`).
    #[must_use]
    pub fn running_task(&self, ws: &str, agent: &str) -> Option<(String, String)> {
        // The most recently started task (queue tail) is the one the agent is
        // most likely mid-turn on.
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(ws.to_owned(), agent.to_owned()))
            .and_then(|tasks| tasks.last().cloned())
    }

    /// Start a workflow instance for `(ws, graph_id)` (from the fleet) and
    /// publish its ready nodes. Journaled (M3) so a restart restores it; a
    /// second start while an instance exists is a no-op.
    ///
    /// # Errors
    /// Unknown graph or invalid graph data.
    pub async fn start_graph(
        &self,
        ws: &str,
        graph_id: &str,
        vars: BTreeMap<String, String>,
    ) -> Result<()> {
        // M3 dedupe: never start twice while an instance is live.
        {
            let instances =
                self.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if instances.contains_key(&(ws.to_owned(), graph_id.to_owned())) {
                tracing::debug!(ws, graph = graph_id, "graph already running; no-op");
                return Ok(());
            }
        }
        let instance = self.load_instance(graph_id).await?;
        {
            let mut instances =
                self.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            instances.insert((ws.to_owned(), graph_id.to_owned()), instance.clone());
        }
        // Journal the start before publishing readiness (M3).
        {
            let mut fleet = self.fleet.lock().await;
            if let Err(e) = fleet.record_workflow_start(ws, graph_id, &vars) {
                tracing::error!(ws, graph = graph_id, error = %e, "journal workflow start failed");
            }
        }
        for node in instance.ready() {
            self.vars
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((ws.to_owned(), graph_id.to_owned(), node.id.clone()), vars.clone());
            self.handle_event(
                ws,
                WorkflowEvent::NodeReady { graph: graph_id.to_owned(), node: node.id.clone() },
            )
            .await;
        }
        Ok(())
    }

    /// Rebuild an instance from the fleet's graph data.
    async fn load_instance(&self, graph_id: &str) -> Result<Workflow> {
        let fleet = self.fleet.lock().await;
        let graph = fleet.graph(graph_id).context("unknown graph")?;
        Workflow::parse_json(&graph.data).context("parse graph data")
    }

    /// Handle a bus event.
    pub async fn handle(&self, event: BusEvent) {
        if let BusEvent::Signal(signal) = event {
            self.on_signal(signal).await;
        }
        // Workflow-related HumanEvent::Commands are routed by the command
        // dispatcher (F4) in `run()`; the old `start` stub is removed.
    }

    /// Route a scoped signal.
    pub async fn on_signal(&self, signal: Signal) {
        let Signal::SessionIdle { ws, agent } = signal else { return };
        self.on_idle(&ws, &agent).await;
    }

    /// An agent finished a turn: if it was working on a node, resolve the ACK
    /// and advance the workflow.
    async fn on_idle(&self, ws: &str, agent: &str) {
        let tasks = {
            let running = self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            running.get(&(ws.to_owned(), agent.to_owned())).cloned().unwrap_or_default()
        };
        if tasks.is_empty() {
            tracing::debug!(ws, agent, "on_idle: no running task for agent");
            return;
        }
        let ack = match self.resolve_ack(ws, agent).await {
            Ok(ack) => ack,
            Err(e) => {
                tracing::warn!(ws, agent, error = %e, "ack resolution failed");
                return;
            }
        };
        let Some(ack) = ack else {
            // Layered resolver found no ACK. Fall back to `done_when.match`
            // (review finding 1): complete any running node whose pattern
            // matches the agent's last output. Without this a match-only
            // node stalls until timeout.
            let text = self.last_output(ws, agent).await;
            let matched = if text.is_empty() {
                false
            } else {
                self.apply_match_fallback(ws, &tasks, &text).await
            };
            // This is the single most useful line for diagnosing a stalled
            // workflow — visible at the default log level.
            tracing::warn!(
                ws,
                agent,
                nodes = ?tasks,
                matched,
                "on_idle: no resolvable ack; match fallback {}", if matched { "matched" } else { "did not match" }
            );
            return;
        };
        tracing::info!(ws, agent, task = %ack.task_id, status = ?ack.status, "ack resolved; advancing node");
        // Publish + apply to every distinct graph holding a running node
        // (one agent may carry nodes from several workflows).
        let graphs: std::collections::BTreeSet<String> =
            tasks.iter().map(|(g, _)| g.clone()).collect();
        for graph in graphs {
            self.bus.publish(BusEvent::Workflow(supervisor_core::dag::WorkflowEvent::Ack {
                graph: graph.clone(),
                ack: ack.clone(),
            }));
            let events = {
                let mut instances =
                    self.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(instance) = instances.get_mut(&(ws.to_owned(), graph.clone())) else {
                    continue;
                };
                instance.apply_ack(&ack)
            };
            for event in events {
                self.handle_event(ws, event).await;
            }
        }
    }

    /// The agent's last output text, for `done_when.match` fallback.
    async fn last_output(&self, ws: &str, agent: &str) -> String {
        let Ok((driver, agent_ref)) = self.drivers.for_agent(ws, agent).await else {
            return String::new();
        };
        driver.read_last_output(&agent_ref, 20).await.unwrap_or_default()
    }

    /// Apply `done_when.match` for every distinct graph in `tasks` against
    /// `text`, completing any running node whose pattern matches (review
    /// finding 1). Returns whether anything matched. Testable without a
    /// driver: callers feed the text in.
    async fn apply_match_fallback(&self, ws: &str, tasks: &[RunningTask], text: &str) -> bool {
        let graphs: std::collections::BTreeSet<String> =
            tasks.iter().map(|(g, _)| g.clone()).collect();
        let mut matched = false;
        for graph in graphs {
            let events = {
                let mut instances =
                    self.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(instance) = instances.get_mut(&(ws.to_owned(), graph.clone())) else {
                    continue;
                };
                instance.apply_match(text)
            };
            if !events.is_empty() {
                matched = true;
            }
            for event in events {
                self.handle_event(ws, event).await;
            }
        }
        matched
    }

    /// The layered ACK resolution for an agent's last output.
    async fn resolve_ack(
        &self,
        ws: &str,
        agent: &str,
    ) -> Result<Option<supervisor_core::ack::Ack>> {
        let (driver, agent_ref) = self
            .drivers
            .for_agent(ws, agent)
            .await
            .with_context(|| format!("driver for {ws}/{agent}"))?;
        let structured = driver.read_structured(&agent_ref).await.ok().flatten();
        let text = driver.read_last_output(&agent_ref, 20).await.unwrap_or_default();
        let structured_json = structured.map(|v| v.to_string());
        Ok(resolve_ack(structured_json.as_deref(), &text))
    }

    /// A ready node: resolve the role to an agent and deliver the start
    /// message into its inbox.
    async fn on_ready(&self, ws: &str, graph: &str, node: &str) {
        let instance = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(ws.to_owned(), graph.to_owned()))
            .cloned();
        let Some(instance) = instance else { return };
        let Some(node_def) = instance.node(node) else { return };
        let vars = self
            .vars
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(ws.to_owned(), graph.to_owned(), node.to_owned()))
            .cloned()
            .unwrap_or_default();
        let rendered = instance.render_start(node, &vars).unwrap_or_default();

        let roster = {
            let fleet = self.fleet.lock().await;
            fleet
                .agents(ws)
                .map(|a| RosterEntry {
                    agent_id: a.agent_id.clone(),
                    role: a.role.clone(),
                    state: a.state,
                    session_id: a.session_id.clone(),
                    inbox_depth: fleet.inbox_depth(ws, &a.agent_id),
                })
                .collect::<Vec<_>>()
        };
        let instance = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(ws.to_owned(), graph.to_owned()))
            .cloned();
        let Some(instance) = instance else { return };
        let target = instance.resolve_target(node, &roster);
        let agent_id = match target {
            RoleResolution::Target(agent_id) => agent_id,
            RoleResolution::MissingRole { role } => {
                tracing::warn!(ws, graph, node, role, "node holds: no agent with this role");
                self.bus.publish(BusEvent::Workflow(WorkflowEvent::MissingRole {
                    graph: graph.to_owned(),
                    node: node.to_owned(),
                    role,
                }));
                return;
            }
        };

        // Deliver the start message.
        let entry = InboxEntry {
            id: format!("w_{}", supervisor_core::time::new_ulid()),
            workspace_id: ws.to_owned(),
            agent_id: agent_id.clone(),
            priority: Priority::Normal,
            body: rendered,
            from: "workflow".to_owned(),
            kind: "instruction".to_owned(),
            in_reply_to: None,
            ack_for: Some(node.to_owned()),
            delivered: false,
            delivered_at: None,
            created_at: now_rfc3339(),
        };
        {
            let mut fleet = self.fleet.lock().await;
            if fleet.workspace(ws).is_some_and(|w| w.state != WorkspaceState::On) {
                tracing::warn!(ws, "workspace not on; start message queued");
            }
            if let Err(e) = fleet.enqueue_inbox(&entry) {
                tracing::error!(ws, agent = %agent_id, error = %e, "enqueue start message failed");
                return;
            }
        }
        self.bus.publish(BusEvent::Inbox(InboxEvent::Enqueued { entry }));
        self.bus.publish(BusEvent::Fleet(FleetEvent::AgentState {
            workspace_id: ws.to_owned(),
            agent_id: agent_id.clone(),
            state: supervisor_core::types::AgentState::Working,
        }));

        // Mark the node Running in the in-memory instance so a later ACK can
        // complete it (on_ready previously only persisted the DB row; the
        // instance stayed Ready forever and apply_ack never matched). The
        // NodeStarted event is not routed through handle_event (that would
        // recurse); the Running state is persisted below.
        {
            let mut instances =
                self.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(instance) = instances.get_mut(&(ws.to_owned(), graph.to_owned())) {
                let _ = instance.start(node);
            }
        }
        self.track_running_task(ws, graph, node, &agent_id, node_def.timeout_secs);
        self.persist_node(ws, graph, node, NodeState::Running).await;
        tracing::info!(ws, graph, node, agent = %agent_id, "node started (start message delivered)");
    }

    /// Record the agent's responsibility for a running node and its timeout
    /// deadline. The slot is a queue: a second workflow on the same agent
    /// appends rather than overwriting (review finding 4). Dedupe so a
    /// restore re-delivery does not double-book the task.
    fn track_running_task(
        &self,
        ws: &str,
        graph: &str,
        node: &str,
        agent_id: &str,
        timeout_secs: Option<u64>,
    ) {
        {
            let mut running =
                self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let queue = running.entry((ws.to_owned(), agent_id.to_owned())).or_default();
            if !queue.iter().any(|(g, n)| g == graph && n == node) {
                queue.push((graph.to_owned(), node.to_owned()));
            }
        }
        if let Some(timeout_secs) = timeout_secs {
            self.deadlines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
                (ws.to_owned(), graph.to_owned(), node.to_owned()),
                Instant::now() + Duration::from_secs(timeout_secs),
            );
        }
    }

    /// Handle one workflow event with workspace context.
    async fn handle_event(&self, ws: &str, event: WorkflowEvent) {
        match event {
            WorkflowEvent::NodeReady { graph, node } => self.on_ready(ws, &graph, &node).await,
            WorkflowEvent::NodeStarted { graph, node } => {
                self.persist_node(ws, &graph, &node, NodeState::Running).await;
            }
            WorkflowEvent::NodeDone { graph, node, .. } => {
                self.clear_running(ws, &node);
                self.persist_node(ws, &graph, &node, NodeState::Done).await;
                tracing::info!(ws, graph, node, "node done");
            }
            WorkflowEvent::NodeFailed { graph, node } => {
                self.clear_running(ws, &node);
                self.persist_node(ws, &graph, &node, NodeState::Failed).await;
                tracing::warn!(ws, graph, node, "node failed");
            }
            WorkflowEvent::NodeBlocked { graph, node, .. } => {
                self.clear_running(ws, &node);
                self.persist_node(ws, &graph, &node, NodeState::Blocked).await;
                tracing::warn!(ws, graph, node, "node blocked");
            }
            WorkflowEvent::NodeNeedsDecision { graph, node } => {
                self.clear_running(ws, &node);
                self.persist_node(ws, &graph, &node, NodeState::NeedsDecision).await;
                tracing::warn!(ws, graph, node, "node needs decision (timeout or failed ack)");
            }
            WorkflowEvent::LoopBack { graph, node, target, .. } => {
                self.clear_running(ws, &node);
                self.persist_node(ws, &graph, &node, NodeState::Ready).await;
                self.on_ready(ws, &graph, &target).await;
            }
            WorkflowEvent::Ack { .. } | WorkflowEvent::MissingRole { .. } => {}
        }
    }

    /// Persist a node state row (journal-first). Preserves the original
    /// `started_at` and `attempt` so the DB/fleet.json projection does not
    /// lose history on every transition (review finding 6); only the first
    /// transition stamps `started_at`.
    async fn persist_node(&self, ws: &str, graph: &str, node: &str, state: NodeState) {
        let (existing_started_at, existing_attempt) = {
            let fleet = self.fleet.lock().await;
            match fleet.node_state(graph, node) {
                Some(row) => (row.started_at.clone(), row.attempt),
                None => (None, 0),
            }
        };
        let row = NodeStateRow {
            graph_id: graph.to_owned(),
            node_id: node.to_owned(),
            state,
            attempt: existing_attempt,
            started_at: existing_started_at.or_else(|| Some(now_rfc3339())),
            finished_at: None,
            error: None,
        };
        let mut fleet = self.fleet.lock().await;
        if let Err(e) = fleet.set_node_state(&row) {
            tracing::error!(ws, graph, node, error = %e, "persist node state failed");
        }
    }

    /// Forget the task an agent was working on (all agents, any graph).
    fn clear_running(&self, ws: &str, node: &str) {
        let mut running = self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for queue in running.values_mut() {
            queue.retain(|(w, n)| w != ws || n != node);
        }
        running.retain(|_key, queue| !queue.is_empty());
        let mut deadlines =
            self.deadlines.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        deadlines.retain(|(w, _g, n), _| w != ws || n != node);
    }

    /// Timeout sweep: running nodes past their per-node timeout move to
    /// `needs_decision`.
    async fn sweep_timeouts(&self) {
        let now = Instant::now();
        let expired: Vec<(String, String, String)> = self
            .deadlines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .map(|((w, g, n), _)| (w.clone(), g.clone(), n.clone()))
            .collect();
        for (ws, graph_id, node_id) in expired {
            self.deadlines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&(
                ws.clone(),
                graph_id.clone(),
                node_id.clone(),
            ));
            let events = {
                let mut instances =
                    self.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                match instances.get_mut(&(ws.clone(), graph_id.clone())) {
                    Some(instance) => instance.timeout(&node_id),
                    None => Vec::new(),
                }
            };
            for event in events {
                self.handle_event(&ws, event).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::cmux::{CmuxClient, CmuxHandle, CmuxWorkspace};
    use std::path::Path;

    #[test]
    fn running_tasks_queue_never_overwrites() {
        // Review finding 4: a second workflow on the same agent appends to
        // the queue; the first task is not stranded.
        let mut map: HashMap<(String, String), Vec<RunningTask>> = HashMap::new();
        let queue = map.entry(("iot".to_owned(), "dev_01".to_owned())).or_default();
        queue.push(("feature_lifecycle".to_owned(), "dev".to_owned()));
        queue.push(("bug_flow".to_owned(), "fix".to_owned()));
        assert_eq!(queue.len(), 2, "a second workflow appends, it does not overwrite");
        assert_eq!(queue.last(), Some(&("bug_flow".to_owned(), "fix".to_owned())));
    }

    struct FakeCmux;

    #[async_trait::async_trait]
    impl CmuxClient for FakeCmux {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn list_workspaces(&self) -> Result<Vec<CmuxWorkspace>> {
            Ok(Vec::new())
        }
        async fn new_workspace(&self, name: &str, _cwd: &Path) -> Result<String> {
            Ok(format!("workspace:{name}"))
        }
        async fn new_surface(&self, _ws: &CmuxHandle, _wd: &Path) -> Result<String> {
            Ok("surface:1".to_owned())
        }
        async fn send_cmd(&self, _ws: &CmuxHandle, _s: &CmuxHandle, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn focus_pane(&self, _ws: &CmuxHandle, _p: &CmuxHandle) -> Result<()> {
            Ok(())
        }
        async fn select_workspace(&self, _ws: &CmuxHandle) -> Result<()> {
            Ok(())
        }
        async fn close_surface(&self, _ws: &CmuxHandle, _s: &CmuxHandle) -> Result<()> {
            Ok(())
        }
        async fn close_workspace(&self, _ws: &CmuxHandle) -> Result<()> {
            Ok(())
        }
        async fn read_screen(&self, _ws: &CmuxHandle, _s: &CmuxHandle) -> Result<String> {
            Ok(String::new())
        }
        async fn send(&self, _ws: &CmuxHandle, _s: &CmuxHandle, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn send_key(&self, _ws: &CmuxHandle, _s: &CmuxHandle, _key: &str) -> Result<()> {
            Ok(())
        }
        async fn notify(&self, _ws: &CmuxHandle, _t: &str, _b: &str) -> Result<()> {
            Ok(())
        }
    }

    fn test_runner(fleet: Arc<AsyncMutex<Fleet>>) -> Arc<WorkflowRunner> {
        let drivers = Arc::new(crate::clients::registry::DriverRegistry::new(
            Arc::clone(&fleet),
            "secret".to_owned(),
        ));
        let workspaces = Arc::new(crate::services::workspace::WorkspaceManager::new(
            crate::services::workspace::ManagerDeps {
                fleet: Arc::clone(&fleet),
                cmux: Arc::new(FakeCmux),
                bus: crate::bus::shared(),
                opencode_bin: "opencode".to_owned(),
                graceful_timeout: Duration::from_secs(1),
                secret: "secret".to_owned(),
                shutdown: CancellationToken::new(),
                allocator: supervisor_core::PortAllocator::default_allocator(),
            },
        ));
        Arc::new(WorkflowRunner::new(
            fleet,
            drivers,
            workspaces,
            crate::bus::shared(),
            CancellationToken::new(),
        ))
    }

    #[tokio::test]
    async fn rule_command_applies_a_manager_ruling() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        // Register the graph.
        {
            let mut f = fleet.lock().await;
            let graph = supervisor_core::types::Graph {
                id: "g".to_owned(),
                name: "g".to_owned(),
                data: r#"{"id":"g","name":"g","nodes":[
                    {"id":"dev","role":"dev","start_template":"do it","done_when":{"ack":"dev"},"on_error":"delegate"}
                ]}"#
                .to_owned(),
                version: 1,
                active: true,
                updated_at: "t".to_owned(),
            };
            f.upsert_graph(&graph).unwrap();
        }
        let runner = test_runner(Arc::clone(&fleet));
        // Insert a running instance directly and force the node to
        // NeedsDecision.
        {
            let mut instances =
                runner.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let instance = Workflow::parse_json(
                r#"{"id":"g","name":"g","nodes":[
                    {"id":"dev","role":"dev","start_template":"do it","done_when":{"ack":"dev"},"on_error":"delegate"}
                ]}"#,
            )
            .unwrap();
            instances.insert(("iot".to_owned(), "g".to_owned()), instance);
            instances.get_mut(&("iot".to_owned(), "g".to_owned())).unwrap().start("dev").unwrap();
            let _ = instances.get_mut(&("iot".to_owned(), "g".to_owned())).unwrap().fail("dev");
        }
        runner
            .on_command(
                "rule",
                &["iot".to_owned(), "g".to_owned(), "dev".to_owned(), "done".to_owned()],
            )
            .await;
        let instances = runner.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let instance = instances.get(&("iot".to_owned(), "g".to_owned())).unwrap();
        assert_eq!(
            instance.state("dev"),
            Some(NodeState::Done),
            "a done ruling completes a NeedsDecision node"
        );
    }

    #[tokio::test]
    async fn running_task_is_queried_after_start() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        let runner = test_runner(Arc::clone(&fleet));
        // A graph with one root node and a matching idle agent in the fleet.
        {
            let mut f = fleet.lock().await;
            let ws = supervisor_core::types::Workspace {
                id: "iot".to_owned(),
                path: "/x/iot".to_owned(),
                port: Some(4101),
                server_pid: None,
                state: supervisor_core::types::WorkspaceState::On,
                cmux_ws: Some("w".to_owned()),
                layout_path: None,
                updated_at: "t".to_owned(),
            };
            f.upsert_workspace(&ws).unwrap();
            f.upsert_agent(&supervisor_core::types::Agent {
                workspace_id: "iot".to_owned(),
                agent_id: "dev_01".to_owned(),
                role: "dev".to_owned(),
                model: None,
                session_id: Some("s1".to_owned()),
                driver: supervisor_core::types::DriverKind::Opencode,
                mode: supervisor_core::types::AgentMode::Background,
                state: supervisor_core::types::AgentState::Idle,
                confidence: 1.0,
            })
            .unwrap();
            let graph = supervisor_core::types::Graph {
                id: "g".to_owned(),
                name: "g".to_owned(),
                data: r#"{"id":"g","name":"g","nodes":[
                    {"id":"dev","role":"dev","start_template":"do it","done_when":{"ack":"dev"}}
                ]}"#
                .to_owned(),
                version: 1,
                active: true,
                updated_at: "t".to_owned(),
            };
            f.upsert_graph(&graph).unwrap();
        }
        runner.start_graph("iot", "g", BTreeMap::new()).await.unwrap();
        assert_eq!(
            runner.running_task("iot", "dev_01"),
            Some(("g".to_owned(), "dev".to_owned())),
            "on_ready records the task for ack resolution"
        );
    }

    #[tokio::test]
    async fn restore_rebuilds_instances_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        // Register the graph + a matching idle agent.
        {
            let mut f = fleet.lock().await;
            let ws = supervisor_core::types::Workspace {
                id: "iot".to_owned(),
                path: "/x/iot".to_owned(),
                port: Some(4101),
                server_pid: None,
                state: supervisor_core::types::WorkspaceState::On,
                cmux_ws: Some("w".to_owned()),
                layout_path: None,
                updated_at: "t".to_owned(),
            };
            f.upsert_workspace(&ws).unwrap();
            f.upsert_agent(&supervisor_core::types::Agent {
                workspace_id: "iot".to_owned(),
                agent_id: "dev_01".to_owned(),
                role: "dev".to_owned(),
                model: None,
                session_id: Some("s1".to_owned()),
                driver: supervisor_core::types::DriverKind::Opencode,
                mode: supervisor_core::types::AgentMode::Background,
                state: supervisor_core::types::AgentState::Idle,
                confidence: 1.0,
            })
            .unwrap();
            let graph = supervisor_core::types::Graph {
                id: "g".to_owned(),
                name: "g".to_owned(),
                data: r#"{"id":"g","name":"g","nodes":[
                    {"id":"dev","role":"dev","start_template":"do it","done_when":{"ack":"dev"}}
                ]}"#
                .to_owned(),
                version: 1,
                active: true,
                updated_at: "t".to_owned(),
            };
            f.upsert_graph(&graph).unwrap();
        }
        // Start on runner #1 (journals workflow.start + enqueues a start msg).
        {
            let runner = test_runner(Arc::clone(&fleet));
            runner.start_graph("iot", "g", BTreeMap::new()).await.unwrap();
        }
        // Simulate a restart: a fresh runner over the same fleet.
        let runner = test_runner(Arc::clone(&fleet));
        runner.restore().await;
        let instances = runner.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let instance = instances
            .get(&("iot".to_owned(), "g".to_owned()))
            .expect("restore rebuilds the instance");
        // M3: the restored Running node re-readies, then restore() re-publishes
        // readiness → on_ready re-delivers and re-starts it.
        assert_eq!(
            instance.state("dev"),
            Some(NodeState::Running),
            "restore re-delivers the start and re-tracks the task"
        );
        assert_eq!(
            runner.running_task("iot", "dev_01"),
            Some(("g".to_owned(), "dev".to_owned())),
            "restore re-delivers the start and re-tracks the task"
        );
    }

    #[tokio::test]
    async fn match_only_node_completes_via_fallback() {
        // Review finding 1: a node with `done_when.match` (no ack) must
        // complete when the agent's output matches — the daemon's idle path
        // has to call apply_match, not just apply_ack.
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        let runner = test_runner(Arc::clone(&fleet));
        let graph = supervisor_core::types::Graph {
            id: "g".to_owned(),
            name: "g".to_owned(),
            data: r#"{"id":"g","name":"g","nodes":[
                {"id":"test","role":"dev","start_template":"run the suite","done_when":{"match":"^ALL PASSED"},"on_error":"delegate"}
            ]}"#
            .to_owned(),
            version: 1,
            active: true,
            updated_at: "t".to_owned(),
        };
        {
            let mut f = fleet.lock().await;
            f.upsert_graph(&graph).unwrap();
        }
        let instance = Workflow::parse_json(&graph.data).unwrap();
        let mut instance = instance;
        {
            let mut instances =
                runner.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            instances.insert(("iot".to_owned(), "g".to_owned()), instance.clone());
            instance.start("test").unwrap();
            instances.insert(("iot".to_owned(), "g".to_owned()), instance);
        }
        {
            let mut running =
                runner.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            running.insert(
                ("iot".to_owned(), "dev_01".to_owned()),
                vec![("g".to_owned(), "test".to_owned())],
            );
        }
        let matched = runner
            .apply_match_fallback("iot", &[("g".to_owned(), "test".to_owned())], "ALL PASSED")
            .await;
        assert!(matched, "the pattern must match");
        let instances = runner.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            instances.get(&("iot".to_owned(), "g".to_owned())).unwrap().state("test"),
            Some(NodeState::Done),
            "a matched done_when.match completes the node"
        );
    }

    #[tokio::test]
    async fn match_only_node_ignores_non_matching_output() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        let runner = test_runner(Arc::clone(&fleet));
        let graph = supervisor_core::types::Graph {
            id: "g".to_owned(),
            name: "g".to_owned(),
            data: r#"{"id":"g","name":"g","nodes":[
                {"id":"test","role":"dev","start_template":"run","done_when":{"match":"^ALL PASSED"}}
            ]}"#
            .to_owned(),
            version: 1,
            active: true,
            updated_at: "t".to_owned(),
        };
        {
            let mut f = fleet.lock().await;
            f.upsert_graph(&graph).unwrap();
        }
        let mut instance = Workflow::parse_json(&graph.data).unwrap();
        {
            let mut instances =
                runner.instances.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            instances.insert(("iot".to_owned(), "g".to_owned()), instance.clone());
            instance.start("test").unwrap();
            instances.insert(("iot".to_owned(), "g".to_owned()), instance);
        }
        let matched = runner
            .apply_match_fallback("iot", &[("g".to_owned(), "test".to_owned())], "FAILED")
            .await;
        assert!(!matched, "a non-matching output must not complete the node");
    }

    #[tokio::test]
    async fn persist_node_keeps_started_at_and_attempt() {
        // Review finding 6: every transition used to reset started_at/attempt
        // in the DB/fleet.json projection, losing the original start time and
        // rerun count.
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        let runner = test_runner(Arc::clone(&fleet));
        runner.persist_node("iot", "g", "dev", NodeState::Running).await;
        let first = fleet.lock().await.node_state("g", "dev").cloned().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        runner.persist_node("iot", "g", "dev", NodeState::Done).await;
        let second = fleet.lock().await.node_state("g", "dev").cloned().unwrap();
        assert_eq!(
            second.started_at, first.started_at,
            "started_at must be stamped once and preserved"
        );
        assert_eq!(second.attempt, first.attempt, "attempt must not reset on every transition");
    }
}
