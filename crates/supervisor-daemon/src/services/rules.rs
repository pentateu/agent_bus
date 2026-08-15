//! The rule wiring service (C9): runs the offline decision cascade over the
//! [`RuleEngine`] on relevant signals and executes the winning [`Action`].
//!
//! Cascade (§4.10): score data + code rules → highest confidence ≥ threshold
//! wins (data beats code on ties); below threshold / conflict / nothing →
//! escalate to the manager (C11) with the layered decision fallback.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use supervisor_core::event::BusEvent;
use supervisor_core::rules::{Action, Evaluation, Rule, RuleEngine, Situation};
use supervisor_core::signal::Signal;
use supervisor_core::time::new_ulid;
use supervisor_core::types::{InboxEntry, Priority};
use supervisor_core::{DecisionRecord, now_rfc3339};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::bus::Receiver;
use crate::clients::manager::ManagerClient;
use crate::services::workflow::WorkflowRunner;
use crate::services::workspace::WorkspaceManager;
use crate::state::Fleet;

/// The rule service.
pub struct RuleService {
    fleet: Arc<AsyncMutex<Fleet>>,
    bus: crate::bus::SharedBus,
    manager: Arc<ManagerClient>,
    /// F4: node context for the situation + M1 `last_output`.
    runner: Arc<WorkflowRunner>,
    /// M9: `Action::FocusPane` focuses via cmux.
    workspaces: Arc<WorkspaceManager>,
    drivers: Arc<crate::clients::registry::DriverRegistry>,
    secret: String,
    shutdown: CancellationToken,
    engine: Mutex<RuleEngine>,
}

impl RuleService {
    /// Build the service. `threshold` comes from the root config; `secret` is
    /// the opencode server password shared across workspaces.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fleet: Arc<AsyncMutex<Fleet>>,
        runner: Arc<WorkflowRunner>,
        workspaces: Arc<WorkspaceManager>,
        drivers: Arc<crate::clients::registry::DriverRegistry>,
        bus: crate::bus::SharedBus,
        manager: Arc<ManagerClient>,
        secret: String,
        shutdown: CancellationToken,
        threshold: f64,
    ) -> Self {
        Self {
            fleet,
            runner,
            workspaces,
            drivers,
            bus,
            manager,
            secret,
            shutdown,
            engine: Mutex::new(RuleEngine::new(threshold)),
        }
    }

    /// Hot-reload the data rules from a `rules.toml` document.
    ///
    /// # Errors
    /// Invalid rules TOML.
    pub fn reload(&self, toml: &str) -> Result<()> {
        let rules = Rule::parse_toml(toml)?;
        self.engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner).set_rules(rules);
        tracing::info!(count = self.data_rule_count(), "rules reloaded");
        Ok(())
    }

    /// Merge a single rule's TOML block into the engine (hot-reload after a
    /// rule add/apply).
    ///
    /// # Errors
    /// Invalid rules TOML.
    pub fn reload_rules_from(&self, toml: &str) -> Result<()> {
        let rules = Rule::parse_toml(toml)?;
        let mut engine = self.engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut all = engine.data_rules().to_vec();
        for rule in rules {
            if let Some(slot) = all.iter_mut().find(|r| r.id == rule.id) {
                *slot = rule;
            } else {
                all.push(rule);
            }
        }
        engine.set_rules(all);
        Ok(())
    }

    fn data_rule_count(&self) -> usize {
        self.engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner).data_rules().len()
    }

    /// Run the main loop until shutdown.
    pub async fn run(&self) {
        let mut rx: Receiver = self.bus.subscribe();
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                event = rx.recv_or_shutdown() => {
                    match event {
                        Some(event) => self.handle(event).await,
                        None => return,
                    }
                }
            }
        }
    }

    /// Handle a bus event: run the cascade on failure-ish signals.
    pub async fn handle(&self, event: BusEvent) {
        let BusEvent::Signal(signal) = event else { return };
        let Some((ws, agent)) = signal.scope() else { return };
        // Only failures / anomalies trigger the decision cascade.
        let relevant = matches!(
            signal,
            Signal::StepFailed { .. } | Signal::SessionError { .. } | Signal::ToolFailed { .. }
        );
        if !relevant {
            return;
        }
        let situation = self.situation(ws, agent, &signal).await;
        let evaluation = self
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .evaluate(&situation);
        match evaluation {
            Evaluation::Act { decision, rule_id } => {
                self.record_decision(&situation, &decision.action, rule_id).await;
                self.act(&situation, decision.action).await;
            }
            Evaluation::Escalate { candidates } => {
                if candidates.is_empty() {
                    // Uncovered situation → escalate to the manager.
                    let rule_ids = Vec::new();
                    self.escalate(&situation, rule_ids).await;
                } else {
                    let rule_ids = candidates.iter().map(|c| c.rule_id.clone()).collect();
                    self.escalate(&situation, rule_ids).await;
                }
            }
        }
    }

    /// Build a situation snapshot for the engine (F4 node context, M1
    /// `last_output` from the driver).
    async fn situation(&self, ws: &str, agent: &str, signal: &Signal) -> Situation {
        let (state, role, inbox_depth) = {
            let fleet = self.fleet.lock().await;
            let a = fleet.agent(ws, agent);
            (
                a.map(|a| a.state).unwrap_or_default(),
                a.map(|a| a.role.clone()).unwrap_or_default(),
                fleet.inbox_depth(ws, agent),
            )
        };
        // F4: the node the agent is working on, when any.
        let node = self.runner.running_task(ws, agent).map(|(graph, node)| {
            supervisor_core::rules::NodeRef {
                graph,
                node,
                state: supervisor_core::types::NodeState::Running,
            }
        });
        // M1: last output from the driver (degrades to None; rules that key on
        // it simply don't match).
        let last_output = if let Ok((driver, agent_ref)) = self.drivers.for_agent(ws, agent).await {
            driver.read_last_output(&agent_ref, 20).await.ok()
        } else {
            None
        };
        Situation {
            ws: ws.to_owned(),
            agent: agent.to_owned(),
            agent_role: role,
            state,
            state_confidence: 1.0,
            reason: signal_reason(signal),
            signals: vec![signal.clone()],
            node,
            inbox_depth,
            last_output,
        }
    }

    /// Execute a winning action (§4.10).
    pub async fn act(&self, sit: &Situation, action: Action) {
        match action {
            Action::Post { to, body } => self.post_to_agent(sit, to, body).await,
            Action::RespondPermission { permission_id, allow } => {
                if let Ok(client) = self.opencode_client(&sit.ws).await {
                    // The permission belongs to whichever session asked; the
                    // situation's agent session is the safe target.
                    if let Some(session) = self.agent_session(&sit.ws, &sit.agent).await {
                        let _ = client.respond_permission(&session, &permission_id, allow).await;
                    }
                }
            }
            Action::Transition { to } => {
                let mut fleet = self.fleet.lock().await;
                if let Some(mut agent) = fleet.agent(&sit.ws, &sit.agent).cloned() {
                    agent.state = to;
                    let _ = fleet.upsert_agent(&agent);
                }
            }
            Action::StartWorkflow { graph, params } => {
                self.bus.publish(BusEvent::Human(supervisor_core::event::HumanEvent::Command {
                    command: "start".to_owned(),
                    args: vec![
                        sit.ws.clone(),
                        graph,
                        serde_json::to_string(&params).unwrap_or_default(),
                    ],
                }));
            }
            Action::FocusPane { ws, agent } => {
                // M9: actually focus the agent's pane via cmux.
                if let Err(e) = self.workspaces.focus_agent(&ws, &agent).await {
                    tracing::warn!(ws = %ws, agent = %agent, error = %e, "focus pane failed");
                }
            }
            Action::Escalate { reason } => {
                // F4: call the escalation path directly (no re-publish).
                let mut sit = sit.clone();
                sit.reason = Some(reason);
                self.escalate(&sit, Vec::new()).await;
            }
            Action::Noop => {}
        }
    }

    /// Enqueue a rule action as an inbox instruction (shared by `act` and the
    /// manager's `post` rulings — keeps `act`/`escalate` from recursing).
    async fn post_to_agent(&self, sit: &Situation, to: String, body: String) {
        let entry = InboxEntry {
            id: format!("w_{}", new_ulid()),
            workspace_id: sit.ws.clone(),
            agent_id: to,
            priority: Priority::Normal,
            body,
            from: "rule".to_owned(),
            kind: "instruction".to_owned(),
            in_reply_to: None,
            ack_for: None,
            delivered: false,
            delivered_at: None,
            created_at: now_rfc3339(),
        };
        let mut fleet = self.fleet.lock().await;
        if let Err(e) = fleet.enqueue_inbox(&entry) {
            tracing::error!(error = %e, "rule post failed to enqueue");
        }
    }

    /// Escalate to the manager and record/act on the decision.
    async fn escalate(&self, sit: &Situation, rule_ids: Vec<String>) {
        let mut candidates = vec!["rerun".to_owned(), "skip".to_owned(), "delegate".to_owned()];
        candidates.extend(rule_ids);
        match self.manager.escalate(sit, candidates).await {
            Ok(Some(decision)) => {
                if decision.confidence < 0.5 {
                    tracing::warn!(
                        ws = %sit.ws,
                        agent = %sit.agent,
                        confidence = decision.confidence,
                        "manager decision below 0.5; surfacing to the human instead of acting"
                    );
                    return;
                }
                match decision.action.as_str() {
                    "post" => {
                        if let (Some(to), Some(body)) = (decision.to.clone(), decision.body.clone())
                        {
                            self.post_to_agent(sit, to, body).await;
                        }
                    }
                    "rerun" | "skip" | "done" | "split" => {
                        // F4: node rulings reach the DAG with full context:
                        // [ws, graph, node, action, to?, body?]. A ruling needs
                        // a node; without one, log and skip.
                        let Some(node_ref) = &sit.node else {
                            tracing::warn!(ws = %sit.ws, action = %decision.action, "manager ruling has no node context; ignoring");
                            return;
                        };
                        let mut args = vec![
                            sit.ws.clone(),
                            node_ref.graph.clone(),
                            node_ref.node.clone(),
                            decision.action.clone(),
                        ];
                        if let Some(to) = &decision.to {
                            args.push(to.clone());
                        }
                        if let Some(body) = &decision.body {
                            args.push(body.clone());
                        }
                        self.bus.publish(BusEvent::Human(
                            supervisor_core::event::HumanEvent::Command {
                                command: "rule".to_owned(),
                                args,
                            },
                        ));
                    }
                    _ => {}
                }
                self.record_decision(sit, &Action::Noop, "manager".to_owned()).await;
            }
            Ok(None) => {
                tracing::warn!(ws = %sit.ws, agent = %sit.agent, "manager produced no decision; surfacing to dashboard");
            }
            Err(e) => {
                tracing::error!(ws = %sit.ws, error = %e, "manager escalation failed");
            }
        }
    }

    /// Record a decision in the decision log (journal-first). M10: the record
    /// carries an `acted` outcome so bake-back sees a success signal
    /// immediately; a caller may overwrite it with a real result later.
    async fn record_decision(&self, sit: &Situation, action: &Action, rule_id: String) {
        let record = DecisionRecord {
            id: format!("dec_{}", new_ulid()),
            signature: supervisor_core::bakeback::normalized_signature(sit),
            situation: serde_json::to_value(sit).unwrap_or_default(),
            decision: serde_json::to_value(action).unwrap_or_default(),
            outcome: Some(serde_json::json!({ "status": "acted", "success": true })),
            ts: now_rfc3339(),
        };
        let mut fleet = self.fleet.lock().await;
        if let Err(e) = fleet.append_decision(&record) {
            tracing::error!(error = %e, rule = %rule_id, "record decision failed");
        }
    }

    async fn agent_session(&self, ws: &str, agent: &str) -> Option<String> {
        let fleet = self.fleet.lock().await;
        fleet.agent(ws, agent).and_then(|a| a.session_id.clone())
    }

    async fn opencode_client(&self, ws: &str) -> Result<crate::clients::opencode::OpencodeClient> {
        let fleet = self.fleet.lock().await;
        let workspace = fleet.workspace(ws).context("unknown workspace")?;
        let port = workspace.port.context("workspace is off")?;
        crate::clients::opencode::OpencodeClient::new(port, &self.secret)
    }
}

/// The reason string for a failure signal.
fn signal_reason(signal: &Signal) -> Option<String> {
    match signal {
        Signal::StepFailed { error, .. } => error.clone(),
        Signal::SessionError { .. } => Some("session.error".to_owned()),
        Signal::ToolFailed { name, .. } => Some(format!("tool.failed:{name}")),
        _ => None,
    }
}
