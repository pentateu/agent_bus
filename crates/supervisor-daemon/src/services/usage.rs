//! The usage collector (§3.3): turns per-message token usage into `usage` rows.
//!
//! After each `step.ended` (and on a 60s fallback poll), the collector reads
//! the agent's last messages' `usage` via the driver's `read_transcript`,
//! diffs against the last recorded message timestamp per agent, and inserts
//! rows. Idempotent by `(agent, message ts)` — the row id is derived from the
//! message timestamp, so a retry never double-counts.

use std::collections::HashMap;
use std::sync::Arc;

use supervisor_core::event::BusEvent;
use supervisor_core::signal::Signal;
use supervisor_core::types::UsageRow;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::bus::Receiver;
use crate::clients::registry::DriverRegistry;
use crate::state::Fleet;

/// How often the fallback poll runs.
const FALLBACK_POLL: std::time::Duration = std::time::Duration::from_mins(1);

/// The usage collector.
pub struct UsageCollector {
    fleet: Arc<AsyncMutex<Fleet>>,
    drivers: Arc<DriverRegistry>,
    bus: crate::bus::SharedBus,
    shutdown: CancellationToken,
    /// `(ws, agent)` → last recorded message ts (idempotency watermark).
    last_ts: tokio::sync::Mutex<HashMap<(String, String), String>>,
}

impl UsageCollector {
    /// Build the collector.
    #[must_use]
    pub fn new(
        fleet: Arc<AsyncMutex<Fleet>>,
        drivers: Arc<DriverRegistry>,
        bus: crate::bus::SharedBus,
        shutdown: CancellationToken,
    ) -> Self {
        Self { fleet, drivers, bus, shutdown, last_ts: tokio::sync::Mutex::new(HashMap::new()) }
    }

    /// Run the collector until shutdown.
    pub async fn run(&self) {
        let mut rx: Receiver = self.bus.subscribe();
        let mut poll = tokio::time::interval(FALLBACK_POLL);
        poll.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = poll.tick() => self.collect_all().await,
                event = rx.recv_or_shutdown() => {
                    match event {
                        Some(BusEvent::Signal(Signal::StepEnded { ws, agent })) => {
                            self.collect(&ws, &agent).await;
                        }
                        Some(_) => {}
                        None => return,
                    }
                }
            }
        }
    }

    /// Collect usage for every agent on `on` workspaces (fallback poll).
    pub async fn collect_all(&self) {
        let targets = {
            let fleet = self.fleet.lock().await;
            fleet
                .workspaces()
                .filter(|w| w.state == supervisor_core::types::WorkspaceState::On)
                .flat_map(|w| {
                    fleet.agents(&w.id).filter_map(|a| {
                        a.session_id.as_ref().map(|_| (w.id.clone(), a.agent_id.clone()))
                    })
                })
                .collect::<Vec<_>>()
        };
        for (ws, agent) in targets {
            self.collect(&ws, &agent).await;
        }
    }

    /// Read an agent's transcript, diff against the watermark, and insert new
    /// usage rows.
    pub async fn collect(&self, ws: &str, agent: &str) {
        // Read the agent's model + session presence first, and DROP the fleet
        // guard before `for_agent`: that lookup re-acquires the fleet mutex,
        // and a tokio mutex is not reentrant — holding the guard across it
        // deadlocks the daemon on the first poll/StepEnded (the guard never
        // releases, and every fleet-lock user queues behind it forever).
        let (model, has_session) = {
            let fleet = self.fleet.lock().await;
            let a = fleet.agent(ws, agent);
            (a.and_then(|a| a.model.clone()), a.is_some_and(|a| a.session_id.is_some()))
        };
        if !has_session {
            return;
        }
        let Ok((driver, agent_ref)) = self.drivers.for_agent(ws, agent).await else { return };
        let Ok(messages) = driver.read_transcript(&agent_ref, 20).await else { return };
        let mut watermark = {
            let last = self.last_ts.lock().await;
            last.get(&(ws.to_owned(), agent.to_owned())).cloned()
        };
        let mut new_rows = Vec::new();
        for message in &messages {
            let Some(usage) = &message.usage else { continue };
            if message.ts.is_empty() {
                continue;
            }
            let ts = message.ts.as_str();
            if watermark.as_deref().is_some_and(|w| ts <= w) {
                continue;
            }
            // Idempotent id derived from the message timestamp.
            new_rows.push(UsageRow {
                id: format!("u_{}_{}", agent_ref.session_id, ts),
                workspace_id: ws.to_owned(),
                agent_id: agent.to_owned(),
                model: model.clone(),
                ts: ts.to_owned(),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
            });
            watermark = Some(ts.to_owned());
        }
        if new_rows.is_empty() {
            return;
        }
        {
            let mut fleet = self.fleet.lock().await;
            for row in &new_rows {
                if let Err(e) = fleet.insert_usage(row) {
                    tracing::debug!(ws, agent, error = %e, "insert usage failed");
                }
            }
        }
        self.last_ts
            .lock()
            .await
            .insert((ws.to_owned(), agent.to_owned()), watermark.unwrap_or_default());
        tracing::debug!(ws, agent, rows = new_rows.len(), "usage collected");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use supervisor_core::types::{
        Agent, AgentMode, AgentState, DriverKind, Workspace, WorkspaceState,
    };

    /// Regression (real-world catch): `collect()` used to hold the fleet
    /// guard while `for_agent` re-acquired the same mutex (tokio mutexes are
    /// not reentrant). The first 60s poll or the first `StepEnded` signal
    /// then deadlocked the daemon: the guard never released, so every
    /// fleet-lock user (API, projection writer, services) queued behind it
    /// forever and the whole API went silent.
    #[tokio::test]
    async fn collect_never_holds_the_fleet_lock_across_for_agent() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = Arc::new(AsyncMutex::new(Fleet::open(dir.path()).unwrap()));
        {
            let mut f = fleet.lock().await;
            f.upsert_workspace(&Workspace {
                id: "iot".to_owned(),
                path: "/x/iot".to_owned(),
                port: Some(4101),
                server_pid: None,
                state: WorkspaceState::On,
                cmux_ws: None,
                layout_path: None,
                updated_at: "t".to_owned(),
            })
            .unwrap();
            f.upsert_agent(&Agent {
                workspace_id: "iot".to_owned(),
                agent_id: "dev_01".to_owned(),
                role: "dev".to_owned(),
                model: None,
                session_id: Some("ses_x".to_owned()),
                driver: DriverKind::Opencode,
                mode: AgentMode::Foreground,
                state: AgentState::Idle,
                confidence: 1.0,
            })
            .unwrap();
        }
        let collector = UsageCollector::new(
            Arc::clone(&fleet),
            Arc::new(crate::clients::registry::DriverRegistry::new(
                Arc::clone(&fleet),
                "s".to_owned(),
            )),
            crate::bus::shared(),
            tokio_util::sync::CancellationToken::new(),
        );
        // With the bug, `collect` parks forever on its own inner lock; the
        // timeout turns that into a test failure instead of a suite hang.
        // (No opencode server listens on 4101 here, so the transcript read
        // fails fast and `collect` returns.)
        tokio::time::timeout(std::time::Duration::from_secs(2), collector.collect("iot", "dev_01"))
            .await
            .expect("collect() must not deadlock on the fleet lock");
        // The fleet lock must be acquirable again — it was not released while
        // `collect` was in flight pre-fix.
        let guard = tokio::time::timeout(std::time::Duration::from_secs(2), fleet.lock())
            .await
            .expect("fleet lock must be released after collect()");
        drop(guard);
    }
}
