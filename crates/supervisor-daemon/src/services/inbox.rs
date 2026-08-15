//! Queue & delivery (C8): one inbox per `(workspace, agent)`, ordered by
//! `(priority desc, created_at)`, delivered when the agent goes idle.
//!
//! Delivery is **at-least-once**: the entry is journaled before the send and
//! marked delivered only after the transport accepts (204). On crash, replay
//! re-queues undelivered entries; idempotency of a retried prompt is the
//! agent's job (task ids in ACK lines, §10).

use std::sync::Arc;

use anyhow::{Context, Result};
use supervisor_core::event::{BusEvent, FleetEvent, InboxEvent};
use supervisor_core::signal::Signal;
use supervisor_core::types::{InboxEntry, Priority, WorkspaceState};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::bus::Receiver;
use crate::clients::registry::DriverRegistry;
use crate::state::Fleet;

/// The inbox delivery service.
pub struct InboxService {
    fleet: Arc<AsyncMutex<Fleet>>,
    drivers: Arc<DriverRegistry>,
    bus: crate::bus::SharedBus,
    shutdown: CancellationToken,
}

impl InboxService {
    /// Build the service.
    #[must_use]
    pub fn new(
        fleet: Arc<AsyncMutex<Fleet>>,
        drivers: Arc<DriverRegistry>,
        bus: crate::bus::SharedBus,
        shutdown: CancellationToken,
    ) -> Self {
        Self { fleet, drivers, bus, shutdown }
    }

    /// Run the delivery loop until shutdown.
    pub async fn run(&self) {
        let mut rx: Receiver = self.bus.subscribe();
        let mut sweep = tokio::time::interval(std::time::Duration::from_secs(2));
        sweep.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = sweep.tick() => self.deliver_pending().await,
                event = rx.recv_or_shutdown() => {
                    match event {
                        Some(event) => self.handle(event).await,
                        None => return,
                    }
                }
            }
        }
    }

    /// Deliver queued messages to idle agents of `on` workspaces. Closes the
    /// gap where an enqueue-triggered delivery fails transiently and no idle
    /// signal arrives afterwards (the message would otherwise sit forever).
    async fn deliver_pending(&self) {
        let targets = {
            let fleet = self.fleet.lock().await;
            fleet
                .workspaces()
                .filter(|w| w.state == WorkspaceState::On)
                .flat_map(|w| {
                    fleet
                        .agents(&w.id)
                        .filter(|a| a.state == supervisor_core::types::AgentState::Idle)
                        .map(move |a| (w.id.clone(), a.agent_id.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (ws, agent) in targets {
            if let Err(e) = self.deliver_next(&ws, &agent).await {
                // I-5: a permanently-failing delivery (e.g. an unimplemented
                // cmux driver) must be loud, not a silent 2s retry.
                tracing::warn!(ws = %ws, agent = %agent, error = %e, "pending delivery failed");
            }
        }
    }

    /// Drain undelivered entries for a workspace's agents (after `on`).
    ///
    /// # Errors
    /// Any failure while delivering.
    pub async fn drain_workspace(&self, ws: &str) -> Result<()> {
        let agents = {
            let fleet = self.fleet.lock().await;
            fleet.agents(ws).map(|a| a.agent_id.clone()).collect::<Vec<_>>()
        };
        for agent in agents {
            let _ = self.deliver_next(ws, &agent).await;
        }
        Ok(())
    }

    /// Deliver the next queued entry to an idle agent, if the workspace is
    /// `on`. High priority is pulled ahead of normal within the same inbox.
    ///
    /// # Errors
    /// Any failure while delivering.
    pub async fn deliver_next(&self, ws: &str, agent: &str) -> Result<()> {
        let next = {
            let fleet = self.fleet.lock().await;
            if !fleet.workspace(ws).is_some_and(|w| w.state == WorkspaceState::On) {
                return Ok(());
            }
            let mut undelivered = fleet.undelivered(ws, agent);
            undelivered.sort_by_key(|e| (e.priority != Priority::High, e.created_at.clone()));
            undelivered.into_iter().next().cloned()
        };
        let Some(entry) = next else { return Ok(()) };
        self.deliver(&entry).await
    }

    /// Deliver one entry through the driver and mark it delivered on success.
    async fn deliver(&self, entry: &InboxEntry) -> Result<()> {
        let (driver, agent_ref) =
            self.drivers.for_agent(&entry.workspace_id, &entry.agent_id).await.with_context(
                || format!("resolve driver for {}/{}", entry.workspace_id, entry.agent_id),
            )?;
        driver
            .send(&agent_ref, &entry.body, None)
            .await
            .with_context(|| format!("deliver to {}/{}", entry.workspace_id, entry.agent_id))?;
        let delivered_at = supervisor_core::now_rfc3339();
        {
            let mut fleet = self.fleet.lock().await;
            fleet.deliver_inbox(&entry.id)?;
        }
        self.bus
            .publish(BusEvent::Inbox(InboxEvent::Delivered { id: entry.id.clone(), delivered_at }));
        Ok(())
    }

    async fn handle(&self, event: BusEvent) {
        match event {
            BusEvent::Fleet(FleetEvent::WorkspaceState { workspace }) => {
                if workspace.state == WorkspaceState::On {
                    let _ = self.drain_workspace(&workspace.id).await;
                }
            }
            // F1: deliver a freshly-enqueued message immediately. opencode
            // queues prompts serially per session, so a busy agent simply
            // parks it server-side; the idle signal below stays as the
            // backpressure net (and drains a queue that accumulated while the
            // workspace was off/draining).
            BusEvent::Inbox(InboxEvent::Enqueued { entry }) => {
                if let Err(e) = self.deliver_next(&entry.workspace_id, &entry.agent_id).await {
                    tracing::warn!(
                        ws = %entry.workspace_id,
                        agent = %entry.agent_id,
                        error = %e,
                        "enqueue-triggered delivery failed"
                    );
                }
            }
            BusEvent::Signal(
                Signal::SessionIdle { ws, agent }
                | Signal::SessionStatus {
                    ws,
                    agent,
                    status: supervisor_core::types::SessionStatus::Idle,
                },
            ) => {
                if let Err(e) = self.deliver_next(&ws, &agent).await {
                    tracing::warn!(ws = %ws, agent = %agent, error = %e, "idle delivery failed");
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use supervisor_core::types::Agent;

    fn entry(ws: &str, agent: &str, id: &str, priority: Priority, created: &str) -> InboxEntry {
        InboxEntry {
            id: id.to_owned(),
            workspace_id: ws.to_owned(),
            agent_id: agent.to_owned(),
            priority,
            body: format!("body {id}"),
            from: "human".to_owned(),
            kind: "instruction".to_owned(),
            in_reply_to: None,
            ack_for: None,
            delivered: false,
            delivered_at: None,
            created_at: created.to_owned(),
        }
    }

    #[test]
    fn high_priority_sorts_before_normal() {
        let low = entry("w", "a", "1", Priority::Normal, "2026-08-13T00:00:00.000Z");
        let high = entry("w", "a", "2", Priority::High, "2026-08-13T00:00:01.000Z");
        let mut items = [&low, &high];
        items.sort_by_key(|e| (e.priority != Priority::High, e.created_at.clone()));
        assert_eq!(items[0].id, "2", "high first");
        let mut items = [&high, &low];
        items.sort_by_key(|e| (e.priority != Priority::High, e.created_at.clone()));
        assert_eq!(items[0].id, "2", "ordering is stable regardless of input order");
    }

    #[test]
    fn agent_builder_keeps_driver() {
        let a = Agent {
            workspace_id: "w".to_owned(),
            agent_id: "a".to_owned(),
            role: "dev".to_owned(),
            model: None,
            session_id: Some("s".to_owned()),
            driver: supervisor_core::types::DriverKind::Cmux,
            mode: supervisor_core::types::AgentMode::Foreground,
            state: supervisor_core::types::AgentState::Idle,
            confidence: 1.0,
        };
        assert_eq!(a.driver, supervisor_core::types::DriverKind::Cmux);
    }

    // F1: an Enqueued event must trigger a delivery attempt.

    fn service(fleet: Arc<AsyncMutex<Fleet>>) -> InboxService {
        let drivers = Arc::new(crate::clients::registry::DriverRegistry::new(
            Arc::clone(&fleet),
            "secret".to_owned(),
        ));
        InboxService::new(fleet, drivers, crate::bus::shared(), CancellationToken::new())
    }

    async fn queue_one(fleet: &Arc<AsyncMutex<Fleet>>, state: WorkspaceState, queued: InboxEntry) {
        let mut f = fleet.lock().await;
        let ws = supervisor_core::types::Workspace {
            id: "iot".to_owned(),
            path: "/x/iot".to_owned(),
            port: Some(4101),
            server_pid: None,
            state,
            cmux_ws: None,
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
            mode: supervisor_core::types::AgentMode::Foreground,
            state: supervisor_core::types::AgentState::Idle,
            confidence: 1.0,
        })
        .unwrap();
        f.enqueue_inbox(&queued).unwrap();
    }

    #[tokio::test]
    async fn enqueued_event_dispatches_delivery_but_off_workspace_parks_it() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        let queued = entry("iot", "dev_01", "e1", Priority::Normal, "2026-08-13T00:00:00.000Z");
        queue_one(&fleet, WorkspaceState::Off, queued.clone()).await;
        let service = service(fleet.clone());

        service.handle(BusEvent::Inbox(InboxEvent::Enqueued { entry: queued })).await;

        let fleet = fleet.lock().await;
        assert_eq!(fleet.inbox_depth("iot", "dev_01"), 1, "off workspace parks the message");
        assert!(
            fleet.undelivered("iot", "dev_01").iter().all(|e| !e.delivered),
            "at-least-once: nothing marked delivered without a successful send"
        );
    }

    #[tokio::test]
    async fn enqueued_event_on_an_on_workspace_attempts_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        let queued = entry("iot", "dev_01", "e1", Priority::Normal, "2026-08-13T00:00:00.000Z");
        queue_one(&fleet, WorkspaceState::On, queued.clone()).await;
        let service = service(fleet.clone());

        // No opencode server is listening on 4101, so delivery attempts and
        // fails; the message must remain queued (never marked delivered).
        service.handle(BusEvent::Inbox(InboxEvent::Enqueued { entry: queued })).await;

        let fleet = fleet.lock().await;
        assert_eq!(fleet.inbox_depth("iot", "dev_01"), 1, "failed send leaves the message queued");
    }
}
