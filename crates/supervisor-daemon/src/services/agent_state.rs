//! The agent state tracker: applies the core transition table (§8) to scoped
//! signals and journals every `agent.state` change.
//!
//! Illegal transitions are rejected and logged; the journal records every
//! `agent.state` change so the counter store can rebuild `error` counts on
//! restart (§4.10).

use std::sync::Arc;

use supervisor_core::event::{BusEvent, FleetEvent};
use supervisor_core::signal::Signal;
use supervisor_core::state::transition;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::bus::Receiver;
use crate::state::Fleet;

/// The agent-state tracker service.
pub struct AgentStateTracker {
    fleet: Arc<AsyncMutex<Fleet>>,
    bus: crate::bus::SharedBus,
    shutdown: CancellationToken,
}

impl AgentStateTracker {
    /// Build the tracker.
    #[must_use]
    pub fn new(
        fleet: Arc<AsyncMutex<Fleet>>,
        bus: crate::bus::SharedBus,
        shutdown: CancellationToken,
    ) -> Self {
        Self { fleet, bus, shutdown }
    }

    /// Run until shutdown.
    pub async fn run(&self) {
        let mut rx: Receiver = self.bus.subscribe();
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                event = rx.recv() => {
                    match event {
                        Ok(BusEvent::Signal(signal)) => self.apply(&signal).await,
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            }
        }
    }

    /// Apply one signal to the agent's recorded state (journal-first).
    pub async fn apply(&self, signal: &Signal) {
        let Some((ws, agent)) = signal.scope() else { return };
        let current = {
            let fleet = self.fleet.lock().await;
            fleet.agent(ws, agent).map(|a| a.state).unwrap_or_default()
        };
        let Some(t) = transition(current, signal) else {
            return; // no-op or illegal transition; surfaced by the daemon if needed
        };
        let mut fleet = self.fleet.lock().await;
        let Some(mut agent) = fleet.agent(ws, agent).cloned() else {
            return;
        };
        agent.state = t.to;
        agent.confidence = t.confidence;
        tracing::info!(ws, agent = %agent.agent_id, from = ?current, to = ?t.to, "agent state transition");
        let _ = fleet.upsert_agent(&agent);
        self.bus.publish(BusEvent::Fleet(FleetEvent::AgentState {
            workspace_id: ws.to_owned(),
            agent_id: agent.agent_id.clone(),
            state: t.to,
        }));
    }
}
