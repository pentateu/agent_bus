//! The workspace manager (C4): `on` / `off` (graceful) / `resume` with
//! foreground + background agents, adopt-or-kill on recorded ports, and
//! supervisor-owned teardown (§4.3).
//!
//! `opencode serve` is spawned with `.current_dir(project)` — it has **no**
//! `--dir` / `--agent` flags (verified). Foreground agents get a cmux
//! terminal surface per agent running `opencode attach --session <id>`;
//! background agents run headless and are driven through the driver.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use supervisor_core::config::ProjectConfig;
use supervisor_core::types::{Agent, AgentMode, AgentState, Workspace, WorkspaceState};
use supervisor_core::{PortAllocator, now_rfc3339};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::bus::SharedBus;
use crate::clients::cmux::{CmuxClient, CmuxHandle};
use crate::clients::opencode::OpencodeClient;
use crate::clients::sse::{SessionResolver, SseObserver};
use crate::state::Fleet;
use supervisor_core::event::{BusEvent, FleetEvent};

/// How long to wait for `serve` health after spawn.
const HEALTH_RETRY: Duration = Duration::from_secs(30);
/// How long to wait for a server child to exit after killing it.
const KILL_WAIT: Duration = Duration::from_secs(10);

/// The outcome of the adopt-or-kill check for a recorded port (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptOrKill {
    /// The surviving process is ours: PID matches and `/global/health` answers.
    Adopt,
    /// The port is held by something else (or a stale record): kill and
    /// respawn on the **same** port so session ids stay valid.
    Kill,
}

/// Pure adopt-or-kill decision.
#[must_use]
pub fn adopt_or_kill(health_ok: bool, pid_matches: bool) -> AdoptOrKill {
    if health_ok && pid_matches { AdoptOrKill::Adopt } else { AdoptOrKill::Kill }
}

/// A child server process.
pub struct ServerChild {
    child: tokio::process::Child,
}

/// The workspace manager.
pub struct WorkspaceManager {
    fleet: Arc<AsyncMutex<Fleet>>,
    cmux: Arc<dyn CmuxClient>,
    bus: SharedBus,
    opencode_bin: String,
    graceful_timeout: Duration,
    secret: String,
    shutdown: CancellationToken,
    children: Mutex<HashMap<String, ServerChild>>,
    allocator: Mutex<PortAllocator>,
    observers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// M7: `session_id → (ws, agent)` for the SSE resolver (no async lock in
    /// the observer path). Entries may outlive an `off`; harmless.
    session_index: Mutex<HashMap<supervisor_core::types::SessionId, (String, String)>>,
    /// M8/M9: `(ws, agent) → cmux surface handle` for foreground panes.
    panes: Mutex<HashMap<(String, String), CmuxHandle>>,
}

/// Dependencies for building a workspace manager.
pub struct ManagerDeps {
    pub fleet: Arc<AsyncMutex<Fleet>>,
    pub cmux: Arc<dyn CmuxClient>,
    pub bus: SharedBus,
    pub opencode_bin: String,
    pub graceful_timeout: Duration,
    pub secret: String,
    pub shutdown: CancellationToken,
    pub allocator: PortAllocator,
}

impl WorkspaceManager {
    /// Build a manager from deps.
    #[must_use]
    pub fn new(deps: ManagerDeps) -> Self {
        Self {
            fleet: deps.fleet,
            cmux: deps.cmux,
            bus: deps.bus,
            opencode_bin: deps.opencode_bin,
            graceful_timeout: deps.graceful_timeout,
            secret: deps.secret,
            shutdown: deps.shutdown,
            children: Mutex::new(HashMap::new()),
            allocator: Mutex::new(deps.allocator),
            observers: Mutex::new(HashMap::new()),
            session_index: Mutex::new(HashMap::new()),
            panes: Mutex::new(HashMap::new()),
        }
    }

    /// Is a workspace currently `on`?
    #[must_use]
    pub async fn is_on(&self, ws_id: &str) -> bool {
        let fleet = self.fleet.lock().await;
        fleet.workspace(ws_id).is_some_and(|w| w.state == WorkspaceState::On)
    }

    /// Idempotent bring-up (§4.3 `on`).
    ///
    /// # Errors
    /// Any failure to configure, spawn, or attach the workspace.
    pub async fn on(&self, ws_id: &str) -> Result<()> {
        let workspace = {
            let fleet = self.fleet.lock().await;
            fleet.workspace(ws_id).cloned().context("workspace not registered")?
        };
        // A no-op is only a no-op when the recorded server is actually alive:
        // after a daemon restart the process is gone even though the journal
        // still says `on`, so resume must re-verify health (adopt-or-kill).
        if workspace.state == WorkspaceState::On {
            // Adopt-or-kill consistency (review round 2, finding 3): the
            // fall-through path requires BOTH health and a matching recorded
            // PID; the already-on branch must too. Health alone could adopt a
            // recycled-PID server that happens to know our password.
            let (alive, port) = match workspace.recorded_port() {
                Some(port) => {
                    let healthy = match OpencodeClient::new(port, &self.secret) {
                        Ok(client) => client.health().await.unwrap_or(false),
                        Err(_) => false,
                    };
                    let pid_matches =
                        if healthy { self.recorded_pid_matches(ws_id, port).await } else { false };
                    (healthy && pid_matches, Some(port))
                }
                None => (false, None),
            };
            if alive {
                tracing::info!(ws = ws_id, "workspace already on; no-op");
                // Resume contract (§4.3 step 4: re-subscribe SSE). A daemon
                // restart starts with an empty in-memory session index and no
                // observer task; if the server survived, this branch would
                // otherwise leave the workspace with NO observer — no status
                // or idle signals, so agent states freeze and ACKs never
                // resolve. Rebuild the index from the fleet's recorded
                // sessions and start a fresh observer (replacing any stale
                // observer from a previous `on`).
                self.rebuild_session_index(ws_id).await;
                self.stop_observer(ws_id);
                if let Some(port) = port
                    && let Ok(client) = OpencodeClient::new(port, &self.secret)
                {
                    self.start_observer(ws_id, client);
                }
                return Ok(());
            }
            tracing::warn!(ws = ws_id, "workspace marked on but its server is dead; respawning");
        }
        let config = load_project_config(&workspace)?;

        // Determine the port: a recorded port is never switched (adopt-or-kill);
        // otherwise the config's fixed port or the allocator.
        let recorded = workspace.recorded_port();
        let port = match recorded {
            Some(p) => p,
            None => config
                .fixed_port()
                .or_else(|| {
                    self.allocator.lock().unwrap_or_else(std::sync::PoisonError::into_inner).alloc()
                })
                .context("no free ports in the configured range")?,
        };

        let decision = match recorded {
            Some(p) => {
                let client = OpencodeClient::new(p, &self.secret)?;
                let health = client.health().await.unwrap_or(false);
                let pid_matches = self.recorded_pid_matches(ws_id, p).await;
                adopt_or_kill(health, pid_matches)
            }
            None => AdoptOrKill::Kill,
        };

        if decision == AdoptOrKill::Kill {
            // Free the port for our own bind: kill any occupant, then respawn
            // on the same port.
            self.release_port_occupant(port).await;
            self.spawn_server(ws_id, &workspace.path, port).await?;
        }

        let cmux_ws = self
            .cmux
            .new_workspace(ws_id, Path::new(&workspace.path))
            .await
            .with_context(|| format!("cmux new-workspace for {ws_id}"))?;

        let client = OpencodeClient::new(port, &self.secret)?;
        self.wait_for_health(&client).await?;

        self.ensure_sessions(ws_id, &config, &client).await?;
        self.ensure_panes(&workspace.path, ws_id, &cmux_ws, port, &config).await?;

        // Record the on state (journal-first).
        let on_workspace = {
            let mut fleet = self.fleet.lock().await;
            let mut ws = workspace.clone();
            ws.port = Some(port);
            ws.state = WorkspaceState::On;
            ws.cmux_ws = Some(cmux_ws);
            ws.updated_at = now_rfc3339();
            fleet.upsert_workspace(&ws)?;
            if fleet.port_of(ws_id).is_none() {
                fleet.alloc_port(port, ws_id)?;
            }
            ws
        };

        // F2: publish the lifecycle so the inbox's drain-on-on fires.
        self.bus.publish(BusEvent::Fleet(FleetEvent::WorkspaceState { workspace: on_workspace }));

        self.start_observer(ws_id, client);
        tracing::info!(ws = ws_id, port, "workspace on");
        Ok(())
    }

    /// Graceful teardown (§4.3 `off`).
    ///
    /// # Errors
    /// Any failure while draining, killing, or closing panels.
    pub async fn off(&self, ws_id: &str, graceful: bool) -> Result<()> {
        let workspace = {
            let fleet = self.fleet.lock().await;
            fleet.workspace(ws_id).cloned().context("workspace not registered")?
        };
        if workspace.state == WorkspaceState::Off {
            return Ok(());
        }
        let port = workspace.port;

        let draining_workspace = {
            let mut fleet = self.fleet.lock().await;
            let mut ws = workspace.clone();
            ws.state = WorkspaceState::Draining;
            ws.updated_at = now_rfc3339();
            fleet.upsert_workspace(&ws)?;
            ws
        };
        // F2: publish the draining lifecycle.
        self.bus
            .publish(BusEvent::Fleet(FleetEvent::WorkspaceState { workspace: draining_workspace }));

        if graceful {
            self.wait_for_idle(ws_id).await;
        }

        // Close the cmux workspace (which closes its surfaces), then the server.
        if let Some(cmux_ws) = &workspace.cmux_ws {
            let _ = self.cmux.close_workspace(cmux_ws).await;
        }
        self.kill_server(ws_id).await?;
        if let Some(port) = port {
            self.allocator.lock().unwrap_or_else(std::sync::PoisonError::into_inner).free(port);
        }
        self.stop_observer(ws_id);

        let off_workspace = {
            let mut fleet = self.fleet.lock().await;
            let mut ws = workspace.clone();
            ws.state = WorkspaceState::Off;
            ws.cmux_ws = None;
            ws.updated_at = now_rfc3339();
            fleet.upsert_workspace(&ws)?;
            ws
        };
        // F2: publish the off lifecycle.
        self.bus.publish(BusEvent::Fleet(FleetEvent::WorkspaceState { workspace: off_workspace }));
        tracing::info!(ws = ws_id, "workspace off");
        Ok(())
    }

    /// Resume every previously-`on` workspace, serially.
    ///
    /// # Errors
    /// Per-workspace failures are logged and skipped; a fatal failure returns
    /// an error.
    pub async fn resume(&self) -> Result<()> {
        let resume = {
            let fleet = self.fleet.lock().await;
            fleet.resume_list().iter().map(|w| w.id.clone()).collect::<Vec<_>>()
        };
        for ws in resume {
            tracing::info!(ws = %ws, "resuming workspace");
            if let Err(e) = self.on(&ws).await {
                tracing::error!(ws = %ws, error = %e, "resume failed");
            }
        }
        Ok(())
    }

    /// M8: spawn a cmux pane attached to a background agent's session (§4.3).
    /// Returns the `cmux send` line that was run, plus whether a pane was
    /// actually spawned.
    ///
    /// # Errors
    /// The workspace is off, the agent has no session, or there is no cmux
    /// workspace.
    pub async fn attach_agent(&self, ws_id: &str, agent_id: &str) -> Result<(String, bool)> {
        let (session, project, port, cmux_ws) = {
            let fleet = self.fleet.lock().await;
            let ws = fleet.workspace(ws_id).context("workspace not registered")?;
            if ws.state != WorkspaceState::On {
                anyhow::bail!("workspace {ws_id} is not on");
            }
            let session = fleet
                .agent(ws_id, agent_id)
                .and_then(|a| a.session_id.clone())
                .context("agent has no session")?;
            let port = ws.port.context("workspace has no port")?;
            (session, ws.path.clone(), port, ws.cmux_ws.clone())
        };
        let Some(cmux_ws) = cmux_ws else {
            anyhow::bail!("workspace {ws_id} has no cmux workspace");
        };
        let attach = format!("opencode attach http://127.0.0.1:{port} --session {session}");
        match self.cmux.new_surface(&cmux_ws, Path::new(&project)).await {
            Ok(surface) => {
                self.cmux.send_cmd(&cmux_ws, &surface, &attach).await?;
                self.panes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert((ws_id.to_owned(), agent_id.to_owned()), surface);
                Ok((attach, true))
            }
            Err(e) => {
                // Fall back to returning the attach command (M8: spawn when
                // cmux is available, else the string).
                tracing::warn!(ws = ws_id, agent = agent_id, error = %e, "attach pane spawn failed; returning command only");
                Ok((attach, false))
            }
        }
    }

    /// M9: focus an agent's foreground pane.
    ///
    /// # Errors
    /// The workspace has no cmux workspace, or no pane is recorded for the
    /// agent.
    pub async fn focus_agent(&self, ws_id: &str, agent_id: &str) -> Result<()> {
        let cmux_ws = {
            let fleet = self.fleet.lock().await;
            fleet
                .workspace(ws_id)
                .and_then(|w| w.cmux_ws.clone())
                .context("workspace has no cmux workspace")?
        };
        let surface = self
            .panes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(ws_id.to_owned(), agent_id.to_owned()))
            .cloned()
            .context("no recorded pane for this agent")?;
        self.cmux.focus_pane(&cmux_ws, &surface).await
    }

    /// Kill every workspace server (daemon shutdown, review finding 5).
    /// Covers both spawned children and adopted/orphaned servers on recorded
    /// ports — an adopted server is not a tracked child, so it would
    /// otherwise orphan on SIGTERM (adopt-or-kill recovers it next start, but
    /// it consumes resources meanwhile).
    pub async fn shutdown(&self) {
        let ws: Vec<String> = {
            self.children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .keys()
                .cloned()
                .collect()
        };
        for ws in ws {
            let _ = self.kill_server(&ws).await;
        }
        // Adopted servers: kill the process on every recorded non-off port.
        let ports: Vec<u16> = {
            let fleet = self.fleet.lock().await;
            fleet
                .workspaces()
                .filter(|w| w.state != WorkspaceState::Off)
                .filter_map(Workspace::recorded_port)
                .collect()
        };
        for port in ports {
            if let Some(pid) = process_pid_on_port(port).await {
                kill_pid(pid).await;
            }
        }
    }

    // --- internals ----------------------------------------------------------

    /// Does the process listening on `port` match the workspace's recorded
    /// PID? The PID is compared directly so a recycled PID (an unrelated
    /// process that grabbed the port after our server died) is never adopted
    /// (§4.3, adopt-or-kill).
    async fn recorded_pid_matches(&self, ws_id: &str, port: u16) -> bool {
        let recorded = {
            let fleet = self.fleet.lock().await;
            fleet.workspace(ws_id).and_then(Workspace::recorded_pid)
        };
        let Some(recorded) = recorded else { return false };
        process_pid_on_port(port).await.is_some_and(|actual| actual == recorded)
    }

    /// Kill whatever occupies `port` so our server can bind it. This is the
    /// orphan-kill path: the occupant is not a child of ours.
    async fn release_port_occupant(&self, port: u16) {
        let ws = self.fleet.lock().await.workspace_for_port(port).map(str::to_owned);
        if let Some(ws) = ws {
            let _ = self.kill_server(&ws).await;
        } else {
            tracing::warn!(port, "port occupied by an unknown process; killing it");
            if let Some(pid) = process_pid_on_port(port).await {
                kill_pid(pid).await;
            }
        }
    }

    /// Spawn `opencode serve` as a child with `.current_dir(project)`.
    async fn spawn_server(&self, ws_id: &str, project: &str, port: u16) -> Result<()> {
        let mut command = tokio::process::Command::new(&self.opencode_bin);
        command
            .args(["serve", "--port", &port.to_string(), "--hostname", "127.0.0.1"])
            .current_dir(project)
            .env("OPENCODE_SERVER_PASSWORD", &self.secret);
        let child = command
            .spawn()
            .with_context(|| format!("spawn {}/serve on port {port}", self.opencode_bin))?;
        let pid = child.id();
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ws_id.to_owned(), ServerChild { child });
        if let Some(pid) = pid {
            let mut fleet = self.fleet.lock().await;
            if let Some(ws) = fleet.workspace(ws_id).cloned() {
                let mut ws = ws;
                ws.server_pid = Some(pid);
                ws.updated_at = now_rfc3339();
                let _ = fleet.upsert_workspace(&ws);
            }
        }
        Ok(())
    }

    /// Poll `/global/health` until it answers (up to `HEALTH_RETRY`).
    async fn wait_for_health(&self, client: &OpencodeClient) -> Result<()> {
        wait_for_health(client, HEALTH_RETRY).await
    }

    /// Reuse or create sessions for every roster agent; returns the agents
    /// with their session ids recorded.
    async fn ensure_sessions(
        &self,
        ws_id: &str,
        config: &ProjectConfig,
        client: &OpencodeClient,
    ) -> Result<Vec<Agent>> {
        let mut agents = Vec::new();
        for roster in &config.agent {
            let existing = {
                let fleet = self.fleet.lock().await;
                fleet.agent(ws_id, &roster.id).cloned()
            };
            let recorded_session = existing.as_ref().and_then(|a| a.session_id.clone());
            let session_id = match recorded_session {
                Some(sid) if client.get_session(&sid).await.ok().flatten().is_some() => sid,
                _ => {
                    let title = format!("{ws_id}/{agent_id}", agent_id = roster.id);
                    let session = client
                        .create_session(&title, Some(&roster.role))
                        .await
                        .with_context(|| format!("create session for {}", roster.id))?;
                    session.id
                }
            };
            let agent = Agent {
                workspace_id: ws_id.to_owned(),
                agent_id: roster.id.clone(),
                role: roster.role.clone(),
                model: roster.model.clone(),
                session_id: Some(session_id.clone()),
                driver: roster.driver,
                mode: roster.mode,
                state: AgentState::Spawning,
                confidence: 1.0,
            };
            let mut fleet = self.fleet.lock().await;
            fleet.upsert_agent(&agent)?;
            // M7: keep a cached session→(ws, agent) map for the SSE resolver
            // (the observer must never take an async fleet lock).
            self.session_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(session_id.clone(), (ws_id.to_owned(), roster.id.clone()));
            agents.push(agent);
        }
        Ok(agents)
    }

    /// Foreground agents get a cmux terminal surface running
    /// `opencode attach --session <id>`; background agents get no pane (§4.3).
    async fn ensure_panes(
        &self,
        project: &str,
        ws_id: &str,
        cmux_ws: &CmuxHandle,
        port: u16,
        config: &ProjectConfig,
    ) -> Result<()> {
        for roster in &config.agent {
            if roster.mode != AgentMode::Foreground {
                continue;
            }
            let session = {
                let fleet = self.fleet.lock().await;
                fleet
                    .agent(ws_id, &roster.id)
                    .and_then(|a| a.session_id.clone())
                    .with_context(|| format!("{} has no session id", roster.id))?
            };
            let surface = self
                .cmux
                .new_surface(cmux_ws, Path::new(project))
                .await
                .with_context(|| format!("cmux surface for {}", roster.id))?;
            let attach = format!("opencode attach http://127.0.0.1:{port} --session {session}");
            self.cmux.send_cmd(cmux_ws, &surface, &attach).await?;
            // M8/M9: record the pane so `attach`/`focus` can find it later.
            self.panes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((ws_id.to_owned(), roster.id.clone()), surface);
        }
        Ok(())
    }

    /// Start the SSE observer for the workspace.
    fn start_observer(&self, ws_id: &str, client: OpencodeClient) {
        // M7: the resolver reads the cached session→(ws, agent) std-mutex map
        // instead of taking an async fleet lock — cheap and never drops a
        // signal under contention.
        let index = Arc::new(
            self.session_index.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
        );
        let resolve: SessionResolver =
            Arc::new(move |session_id: &str| index.get(session_id).cloned());
        let observer = SseObserver::new(
            client,
            ws_id.to_owned(),
            resolve,
            Arc::clone(&self.bus),
            self.shutdown.clone(),
        );
        let handle = tokio::spawn(observer.run());
        self.observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ws_id.to_owned(), handle);
        tracing::info!(ws = ws_id, "sse observer started");
    }

    /// Rebuild the session→(ws, agent) resolver index from the fleet's
    /// recorded sessions. A daemon restart starts with an empty in-memory
    /// index; the already-on resume path needs this so the observer's resolver
    /// can map signals to agents again (without re-running session creation).
    async fn rebuild_session_index(&self, ws_id: &str) {
        let sessions = {
            let fleet = self.fleet.lock().await;
            fleet
                .agents(ws_id)
                .filter_map(|a| a.session_id.as_ref().map(|sid| (sid.clone(), a.agent_id.clone())))
                .collect::<Vec<_>>()
        };
        let mut index =
            self.session_index.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (session_id, agent_id) in sessions {
            index.insert(session_id, (ws_id.to_owned(), agent_id));
        }
        tracing::info!(ws = ws_id, count = index.len(), "sse session index rebuilt");
    }

    fn stop_observer(&self, ws_id: &str) {
        if let Some(handle) =
            self.observers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(ws_id)
        {
            handle.abort();
        }
    }

    /// Wait until every *working* agent is idle (or the graceful timeout
    /// elapses). Agents that are merely attached (`spawning` / never started a
    /// turn) have nothing to drain and are not waited on.
    async fn wait_for_idle(&self, ws_id: &str) {
        let deadline = tokio::time::Instant::now() + self.graceful_timeout;
        loop {
            let busy = {
                let fleet = self.fleet.lock().await;
                fleet.agents(ws_id).any(|a| matches!(a.state, AgentState::Working))
            };
            if !busy || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Kill the workspace's server child (SIGTERM, then SIGKILL after 10s).
    async fn kill_server(&self, ws_id: &str) -> Result<()> {
        let child = {
            let mut children =
                self.children.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            children.remove(ws_id)
        };
        if let Some(mut server) = child {
            server.child.start_kill().ok();
            let _ = tokio::time::timeout(KILL_WAIT, server.child.wait()).await;
            let mut fleet = self.fleet.lock().await;
            if let Some(ws) = fleet.workspace(ws_id).cloned() {
                let mut ws = ws;
                ws.server_pid = None;
                ws.updated_at = now_rfc3339();
                let _ = fleet.upsert_workspace(&ws);
            }
        }
        Ok(())
    }
}

/// Load a project's `supervisor.toml` from its layout path (or the project
/// root).
fn load_project_config(workspace: &Workspace) -> Result<ProjectConfig> {
    let candidates = [
        workspace.layout_path.as_deref().map(PathBuf::from),
        Some(Path::new(&workspace.path).join("supervisor.toml")),
    ]
    .into_iter()
    .flatten();
    for candidate in candidates {
        if candidate.exists() {
            let contents = std::fs::read_to_string(&candidate)
                .with_context(|| format!("reading {}", candidate.display()))?;
            return ProjectConfig::parse(&contents)
                .with_context(|| format!("parsing {}", candidate.display()));
        }
    }
    bail!("no supervisor.toml found for workspace {} (path {})", workspace.id, workspace.path);
}

/// Poll `/global/health` until it answers (up to `timeout`). Shared by the
/// workspace manager and the F5 supervisor-workspace startup.
///
/// # Errors
/// The server did not become healthy within the timeout.
pub async fn wait_for_health(client: &OpencodeClient, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if client.health().await.unwrap_or(false) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("opencode serve did not become healthy within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The PID of the process listening on `port` (LISTEN state), via `lsof`.
/// `None` when nothing listens or the lookup fails.
pub async fn process_pid_on_port(port: u16) -> Option<u32> {
    let output = tokio::process::Command::new("lsof")
        .args(["-t", "-iTCP", &port.to_string(), "-sTCP:LISTEN"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find_map(|tok| tok.parse::<u32>().ok())
}

/// Kill a PID (orphan path): SIGTERM, then SIGKILL after a short grace.
pub async fn kill_pid(pid: u32) {
    let output =
        tokio::process::Command::new("kill").arg("-TERM").arg(pid.to_string()).output().await;
    if output.map_or(true, |o| !o.status.success()) {
        return;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = tokio::process::Command::new("kill").args(["-KILL", &pid.to_string()]).output().await;
}

/// Shorthand used by callers that want the daemon shutdown token.
#[must_use]
pub fn cancellation() -> CancellationToken {
    CancellationToken::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopt_only_when_pid_matches_and_healthy() {
        assert_eq!(adopt_or_kill(true, true), AdoptOrKill::Adopt);
        assert_eq!(adopt_or_kill(true, false), AdoptOrKill::Kill, "recycled PID must be killed");
        assert_eq!(adopt_or_kill(false, true), AdoptOrKill::Kill, "no health → kill");
        assert_eq!(adopt_or_kill(false, false), AdoptOrKill::Kill);
    }
}
