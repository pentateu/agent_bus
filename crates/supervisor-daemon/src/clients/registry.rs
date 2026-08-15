//! The driver registry: resolves an agent to its [`AgentDriver`] + [`AgentRef`].
//!
//! One opencode client is cached per `on` workspace (its server port); agents
//! on that workspace share it through an [`OpencodeDriver`]. The cmux driver
//! (future, M9) would resolve by pane instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use supervisor_core::types::{AgentId, DriverKind};
use tokio::sync::Mutex as AsyncMutex;

use crate::clients::driver::{AgentDriver, AgentRef};
use crate::clients::opencode::{OpencodeClient, OpencodeDriver};
use crate::state::Fleet;

/// Resolves `(ws, agent)` → driver + agent ref.
pub struct DriverRegistry {
    fleet: Arc<AsyncMutex<Fleet>>,
    secret: String,
    /// `ws` → cached opencode client.
    clients: Mutex<HashMap<String, OpencodeClient>>,
}

impl DriverRegistry {
    /// Build a registry.
    #[must_use]
    pub fn new(fleet: Arc<AsyncMutex<Fleet>>, secret: String) -> Self {
        Self { fleet, secret, clients: Mutex::new(HashMap::new()) }
    }

    /// The driver + ref for an agent. Errors when the workspace is not on or
    /// the agent has no session.
    ///
    /// # Errors
    /// Missing workspace/agent/session, or an invalid client URL.
    pub async fn for_agent(
        &self,
        ws: &str,
        agent: &str,
    ) -> Result<(Arc<dyn AgentDriver>, AgentRef)> {
        let (session_id, driver_kind, port) = {
            let fleet = self.fleet.lock().await;
            let agent = fleet.agent(ws, agent).context("unknown agent")?;
            let session_id = agent.session_id.clone().context("agent has no session")?;
            let workspace = fleet.workspace(ws).context("unknown workspace")?;
            let port = workspace.port.context("workspace is off")?;
            (session_id, agent.driver, port)
        };
        let driver: Arc<dyn AgentDriver> = match driver_kind {
            DriverKind::Opencode => {
                let client = self.client_for(ws, port)?;
                Arc::new(OpencodeDriver::new(client.clone()))
            }
            DriverKind::Cmux => {
                // M9: surfaces are resolved by pane handle; today the
                // supervisor does not yet record pane handles per agent, so a
                // real cmux agent fails fast at resolve time.
                anyhow::bail!("the cmux driver requires a recorded pane for {ws}/{agent} (M9)")
            }
        };
        let agent_ref = AgentRef { ws: ws.to_owned(), agent: agent.to_owned(), session_id };
        Ok((driver, agent_ref))
    }

    /// The cached (or new) opencode client for a workspace's port.
    fn client_for(&self, ws: &str, port: u16) -> Result<OpencodeClient> {
        if let Some(client) =
            self.clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(ws)
        {
            return Ok(client.clone());
        }
        let client = OpencodeClient::new(port, &self.secret)
            .with_context(|| format!("build opencode client for {ws} on {port}"))?;
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ws.to_owned(), client.clone());
        Ok(client)
    }
}

/// A helper for tests: an agent id alias.
pub type AgentKey = (String, AgentId);
