//! The supervisor daemon binary: the launchd headless runtime (no TUI).
//!
//! Startup sequence (§5): load config; open DB; replay journal; ensure the
//! default graphs; ensure the supervisor workspace; bind the loopback API;
//! start observers, delivery, workflow, rules, and ingestion. SIGINT/SIGTERM
//! drains and exits 0.

use std::sync::Arc;

use anyhow::{Context, Result};
use supervisor_core::config::{ProjectConfig, SupervisorConfig};
use supervisor_core::graphs::default_graph;
use supervisor_core::types::{Graph, Workspace, WorkspaceState};
use supervisor_core::{PortAllocator, now_rfc3339};
use tokio::sync::Mutex as AsyncMutex;
use tracing_subscriber::EnvFilter;

use supervisor_daemon::api::{AppState, load_or_create_token, router, token_path};
use supervisor_daemon::bus::{self, SharedBus};
use supervisor_daemon::clients::cmux::ProcessCmux;
use supervisor_daemon::clients::manager::ManagerClient;
use supervisor_daemon::clients::registry::DriverRegistry;
use supervisor_daemon::secrets::{self, secrets_path};
use supervisor_daemon::services::agent_state::AgentStateTracker;
use supervisor_daemon::services::bakeback::BakebackService;
use supervisor_daemon::services::inbox::InboxService;
use supervisor_daemon::services::ingest::IngestionService;
use supervisor_daemon::services::rules::RuleService;
use supervisor_daemon::services::usage::UsageCollector;
use supervisor_daemon::services::workflow::WorkflowRunner;
use supervisor_daemon::services::workspace::{ManagerDeps, WorkspaceManager};
use supervisor_daemon::state::Fleet;

/// The PID file recording the supervisor workspace server (F5).
const SUPERVISOR_SERVE_PID: &str = "supervisor.serve.pid";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("supervisor=info")),
        )
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    runtime.block_on(run())
}

/// The daemon entry point.
#[allow(clippy::too_many_lines)]
async fn run() -> Result<()> {
    let state_dir = default_state_dir();
    tracing::info!(state_dir = %state_dir.display(), "supervisor daemon starting");

    // 1. Config + fleet + secrets + token.
    let config = load_config(&state_dir)?;
    let fleet = Arc::new(AsyncMutex::new(Fleet::open(&state_dir).context("opening fleet state")?));
    let secret = secrets::load_or_create(&secrets_path(&state_dir))
        .context("loading secrets")?
        .server_password;
    let token = load_or_create_token(&token_path(&state_dir)).context("loading API token")?;

    // 2. Ensure default graphs are installed.
    ensure_default_graphs(&fleet).await?;

    // 3. Discover projects: every immediate child of workspace_root carrying a
    // `supervisor.toml` is auto-registered (§5).
    discover_projects(&fleet, &config).await?;

    // 3. The shared event bus.
    let shared_bus: SharedBus = bus::shared();

    // 4. Services.
    let allocator = PortAllocator::new(config.port_range(), config.reserved_ports());
    let manager =
        Arc::new(ManagerClient::connect(config.supervisor.supervisor_workspace_port, &secret)?);
    let drivers = Arc::new(DriverRegistry::new(Arc::clone(&fleet), secret.clone()));
    let shutdown = supervisor_daemon::services::workspace::cancellation();

    let workspaces = Arc::new(WorkspaceManager::new(ManagerDeps {
        fleet: Arc::clone(&fleet),
        cmux: Arc::new(ProcessCmux::new(config.supervisor.cmux_bin.clone())),
        bus: Arc::clone(&shared_bus),
        opencode_bin: config.supervisor.opencode_bin.clone(),
        graceful_timeout: std::time::Duration::from_secs(config.graceful.off_timeout_secs),
        secret: secret.clone(),
        shutdown: shutdown.clone(),
        allocator,
    }));
    let workflows = Arc::new(WorkflowRunner::new(
        Arc::clone(&fleet),
        Arc::clone(&drivers),
        Arc::clone(&workspaces),
        Arc::clone(&shared_bus),
        shutdown.clone(),
    ));
    let inbox = InboxService::new(
        Arc::clone(&fleet),
        Arc::clone(&drivers),
        Arc::clone(&shared_bus),
        shutdown.clone(),
    );
    let tracker =
        AgentStateTracker::new(Arc::clone(&fleet), Arc::clone(&shared_bus), shutdown.clone());
    let rules = Arc::new(RuleService::new(
        Arc::clone(&fleet),
        Arc::clone(&workflows),
        Arc::clone(&workspaces),
        Arc::clone(&drivers),
        Arc::clone(&shared_bus),
        Arc::clone(&manager),
        secret.clone(),
        shutdown.clone(),
        config.rule.threshold,
    ));
    let bakeback = Arc::new(BakebackService::new(
        Arc::clone(&fleet),
        config.bakeback.min_occurrences,
        state_dir.join("rules.toml"),
    ));
    let ingest = Arc::new(IngestionService::new(
        Arc::clone(&fleet),
        Arc::clone(&workflows),
        Arc::clone(&workspaces),
        Arc::clone(&shared_bus),
        shutdown.clone(),
    ));
    if let Err(e) = ingest.discover_adapters().await {
        tracing::warn!(error = %e, "ingestion adapter discovery failed");
    }

    // U5: usage collector (tokens → usage rows; cost on read).
    let usage_collector = Arc::new(UsageCollector::new(
        Arc::clone(&fleet),
        Arc::clone(&drivers),
        Arc::clone(&shared_bus),
        shutdown.clone(),
    ));

    // 4b. F5: the supervisor workspace (`opencode serve :4199`) that hosts the
    // manager (C11) and the supervisor agent (C13). Non-fatal: a failure to
    // bring it up must not kill the daemon — the manager session is lazy and
    // escalations surface to the dashboard instead.
    let supervisor_serve = if config.supervisor.open_supervisor_workspace {
        match ensure_supervisor_workspace(&config, &secret, &state_dir).await {
            Ok(child) => child,
            Err(e) => {
                tracing::error!(error = %e, "supervisor workspace failed to start; continuing without it");
                None
            }
        }
    } else {
        tracing::info!("supervisor workspace disabled (open_supervisor_workspace=false)");
        None
    };

    // 4c. F6: expire stale proposals now, then a daily auto-preview timer.
    if let Err(e) = bakeback.expire_old().await {
        tracing::warn!(error = %e, "bake-back expire on start failed");
    }
    if config.bakeback.auto_preview {
        tokio::spawn({
            let bakeback = Arc::clone(&bakeback);
            let shutdown = shutdown.clone();
            async move {
                let mut timer = tokio::time::interval(std::time::Duration::from_hours(24));
                timer.tick().await; // consume the immediate first tick
                loop {
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        _ = timer.tick() => {
                            match bakeback.preview().await {
                                Ok(created) => {
                                    tracing::info!(count = created.len(), "bake-back auto-preview");
                                }
                                Err(e) => tracing::warn!(error = %e, "bake-back auto-preview failed"),
                            }
                            if let Err(e) = bakeback.expire_old().await {
                                tracing::warn!(error = %e, "bake-back expire failed");
                            }
                        }
                    }
                }
            }
        });
    }

    // 5. Spawn service tasks.
    tokio::spawn({
        let workflows = Arc::clone(&workflows);
        async move { workflows.run().await }
    });
    // M10: low-frequency `fleet.json` projection writer (5s snapshot).
    tokio::spawn({
        let fleet = Arc::clone(&fleet);
        let shutdown = shutdown.clone();
        async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    _ = ticker.tick() => {
                        if let Err(e) = fleet.lock().await.write_projection() {
                            tracing::debug!(error = %e, "fleet.json write failed");
                        }
                    }
                }
            }
        }
    });
    tokio::spawn({
        let inbox = Arc::new(inbox);
        async move { inbox.run().await }
    });
    tokio::spawn({
        let tracker = Arc::new(tracker);
        async move { tracker.run().await }
    });
    tokio::spawn({
        let rules = Arc::clone(&rules);
        async move { rules.run().await }
    });
    tokio::spawn({
        let ingest = Arc::clone(&ingest);
        async move { ingest.run().await }
    });
    tokio::spawn({
        let usage_collector = Arc::clone(&usage_collector);
        async move { usage_collector.run().await }
    });

    // 6. Resume previously-on workspaces.
    if config.supervisor.open_workspaces_on_start
        && let Err(e) = workspaces.resume().await
    {
        tracing::error!(error = %e, "workspace resume failed");
    }

    // 7. Bind the loopback API.
    let app_state = Arc::new(AppState {
        fleet: Arc::clone(&fleet),
        bus: Arc::clone(&shared_bus),
        workspaces: Arc::clone(&workspaces),
        drivers: Arc::clone(&drivers),
        workflows: Arc::clone(&workflows),
        rules: Arc::clone(&rules),
        bakeback: Arc::clone(&bakeback),
        usage_config: config.usage.clone(),
        token,
        state_dir: state_dir.clone(),
        shutdown: shutdown.clone(),
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.supervisor.api_port))
        .await
        .with_context(|| format!("binding API on {}", config.supervisor.api_port))?;
    tracing::info!(port = config.supervisor.api_port, "supervisor API listening");
    let result = axum::serve(listener, router(&app_state))
        .with_graceful_shutdown(shutdown_guard(shutdown.clone()))
        .await
        .context("API server exited unexpectedly");
    // Review finding 5: shutdown must close the per-workspace children (the
    // design's "close children" step), not just the supervisor workspace
    // server. Without this every `opencode serve` orphans on SIGTERM (adopt-
    // or-kill recovers them next start, but they consume resources meanwhile).
    tracing::info!("shutdown: closing workspace servers");
    workspaces.shutdown().await;
    // F5: tear down the supervisor workspace server on shutdown. Covers both
    // the spawned child and an adopted server (adopt path records the PID in
    // the pid file but returns no child handle).
    if let Some(mut child) = supervisor_serve {
        let _ = child.kill().await;
    }
    if let Ok(pid) = std::fs::read_to_string(state_dir.join(SUPERVISOR_SERVE_PID))
        && let Ok(pid) = pid.trim().parse::<u32>()
        && pid != std::process::id()
    {
        supervisor_daemon::services::workspace::kill_pid(pid).await;
    }
    result
}

/// A future that completes on SIGINT or SIGTERM (the launchd/`kill -TERM`
/// signal), then cancels the shutdown token. `tokio::signal::ctrl_c` alone
/// only handles SIGINT — without a SIGTERM handler the daemon dies via the
/// default disposition and never runs the "close children" shutdown path,
/// orphaning every `opencode serve` (review finding 5).
async fn shutdown_guard(shutdown: tokio_util::sync::CancellationToken) {
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    if let (Ok(int), Ok(term)) = (&mut sigint, &mut sigterm) {
        tokio::select! {
            _ = int.recv() => tracing::info!("SIGINT received; shutting down gracefully"),
            _ = term.recv() => tracing::info!("SIGTERM received; shutting down gracefully"),
        }
    } else {
        tracing::warn!("cannot install SIGINT/SIGTERM handlers; falling back to ctrl_c");
        let _ = tokio::signal::ctrl_c().await;
    }
    shutdown.cancel();
}

/// F5: ensure the supervisor workspace server (`opencode serve :4199`) is
/// running, adopting a live one on restart (PID + `/global/health`), else
/// killing the occupant and respawning on the same port. Returns the child to
/// keep alive, or `None` when adopted / skipped.
///
/// # Errors
/// The server could not be spawned or did not become healthy.
async fn ensure_supervisor_workspace(
    config: &SupervisorConfig,
    secret: &str,
    state_dir: &std::path::Path,
) -> Result<Option<tokio::process::Child>> {
    use supervisor_daemon::clients::opencode::OpencodeClient;
    use supervisor_daemon::services::workspace::{kill_pid, process_pid_on_port, wait_for_health};

    let port = config.supervisor.supervisor_workspace_port;
    let cwd = expand_home(&config.supervisor.workspace_root);
    std::fs::create_dir_all(&cwd).with_context(|| format!("creating {}", cwd.display()))?;
    let client = OpencodeClient::new(port, secret)?;
    let pid_file = state_dir.join(SUPERVISOR_SERVE_PID);

    // Adopt-or-kill on restart: adopt only if the recorded PID is alive AND
    // the port answers health (a recycled PID is never adopted).
    let recorded =
        std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<u32>().ok());
    let pid_on_port = process_pid_on_port(port).await;
    let healthy = client.health().await.unwrap_or(false);
    let adopt = recorded.is_some_and(|pid| pid_on_port == Some(pid) && healthy);
    if adopt {
        tracing::info!(port, pid = recorded.unwrap_or(0), "supervisor workspace adopted");
        return Ok(None);
    }

    // Kill any occupant (orphan) so our bind succeeds, then respawn.
    if let Some(pid) = process_pid_on_port(port).await {
        tracing::warn!(port, pid, "supervisor workspace port occupied; killing orphan");
        kill_pid(pid).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    let mut command = tokio::process::Command::new(&config.supervisor.opencode_bin);
    command
        .args(["serve", "--port", &port.to_string(), "--hostname", "127.0.0.1"])
        .current_dir(&cwd)
        .env("OPENCODE_SERVER_PASSWORD", secret);
    let child = command.spawn().with_context(|| format!("spawn supervisor serve on {port}"))?;
    if let Some(pid) = child.id() {
        std::fs::write(&pid_file, pid.to_string())
            .with_context(|| format!("writing {}", pid_file.display()))?;
    }
    wait_for_health(&client, std::time::Duration::from_secs(30)).await?;
    tracing::info!(port, cwd = %cwd.display(), "supervisor workspace server up");
    Ok(Some(child))
}

/// Load the root config, defaulting when the file is absent.
fn load_config(state_dir: &std::path::Path) -> Result<SupervisorConfig> {
    let path = state_dir.join("supervisor.toml");
    if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        SupervisorConfig::parse(&contents).with_context(|| format!("parsing {}", path.display()))
    } else {
        Ok(SupervisorConfig::default())
    }
}

/// Install the default graphs into the fleet (idempotent).
async fn ensure_default_graphs(fleet: &Arc<AsyncMutex<Fleet>>) -> Result<()> {
    for id in supervisor_core::graphs::default_graph_ids() {
        let mut guard = fleet.lock().await;
        if guard.graph(id).is_some() {
            continue;
        }
        let workflow = default_graph(id)?;
        let data = if *id == "feature_lifecycle" {
            supervisor_core::FEATURE_LIFECYCLE_JSON
        } else {
            supervisor_core::BUG_FLOW_JSON
        };
        let graph = Graph {
            id: id.to_string(),
            name: workflow.graph().name.clone(),
            data: data.to_owned(),
            version: 1,
            active: true,
            updated_at: now_rfc3339(),
        };
        guard.upsert_graph(&graph)?;
    }
    Ok(())
}

/// Discover projects: every immediate child of the workspace root carrying a
/// `supervisor.toml` is auto-registered as an `off` workspace (§5).
async fn discover_projects(
    fleet: &Arc<AsyncMutex<Fleet>>,
    config: &SupervisorConfig,
) -> Result<()> {
    let root = expand_home(&config.supervisor.workspace_root);
    let Ok(entries) = std::fs::read_dir(&root) else {
        tracing::warn!(root = %root.display(), "workspace root missing; skipping project discovery");
        return Ok(());
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let layout = entry.path().join("supervisor.toml");
        if !layout.exists() {
            continue;
        }
        let contents = match std::fs::read_to_string(&layout) {
            Ok(contents) => contents,
            Err(e) => {
                tracing::warn!(path = %layout.display(), error = %e, "cannot read supervisor.toml");
                continue;
            }
        };
        let project = match ProjectConfig::parse(&contents) {
            Ok(project) => project,
            Err(e) => {
                tracing::warn!(path = %layout.display(), error = %e, "invalid supervisor.toml, skipping");
                continue;
            }
        };
        let mut guard = fleet.lock().await;
        if guard.workspace(&project.project.name).is_some() {
            continue;
        }
        let workspace = Workspace {
            id: project.project.name.clone(),
            path: project.project.path.clone(),
            port: None,
            server_pid: None,
            state: WorkspaceState::Off,
            cmux_ws: None,
            layout_path: Some(layout.to_string_lossy().into_owned()),
            updated_at: now_rfc3339(),
        };
        if let Err(e) = guard.upsert_workspace(&workspace) {
            tracing::error!(path = %layout.display(), error = %e, "register discovered project failed");
        } else {
            tracing::info!(ws = %workspace.id, path = %workspace.path, "discovered project");
        }
    }
    Ok(())
}

/// Expand a leading `~` in a path against `$HOME`.
fn expand_home(path: &std::path::Path) -> std::path::PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home).join(rest)
    } else {
        path.to_path_buf()
    }
}

/// `~/.supervisor` (deviation 10: one supervisor-owned state dir).
#[must_use]
fn default_state_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    std::path::PathBuf::from(home).join(".supervisor")
}
