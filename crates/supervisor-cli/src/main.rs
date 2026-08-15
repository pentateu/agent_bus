//! The `supervisor` command (§4.15): a thin client over the daemon's loopback
//! API. `daemon` is the headless runtime (spawned), `dashboard` attaches to
//! the running daemon.
//!
//! Exit codes (§4.15): 0 success, 1 usage, 2 target not found, 3 daemon
//! unreachable.

use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

use crate::client::{ApiClient, ClientConfig};

mod client;
mod dashboard;

/// The fleet supervisor: owns every managed project's agents.
#[derive(Debug, Parser)]
#[command(name = "supervisor", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// The supervisor state dir (defaults to `~/.supervisor`).
    #[arg(long, global = true)]
    pub state_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the headless daemon runtime (launchd target).
    Daemon,
    /// All workspaces + agent states + queue depth.
    Status,
    /// Idempotent bring-up of a workspace.
    On { project: String },
    /// Graceful off (`--force` skips the wait).
    Off {
        project: String,
        #[arg(long)]
        force: bool,
    },
    /// Restore all on-marked workspaces (serial).
    Resume,
    /// Gracefully stop the daemon (SIGTERM, wait for exit, report).
    Stop,
    /// Start a workflow graph for a workspace (bringing it on if off).
    Start {
        ws: String,
        graph: String,
        /// A `key=value` workflow variable; repeatable.
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
    },
    /// Live one-node acceptance smoke: on → start → root Ready → Running →
    /// ACK → Done (asserts each hop via the node-states API).
    Smoke {
        ws: String,
        graph: String,
        /// Seconds to wait for the chain to progress (default 180).
        #[arg(long, default_value_t = 180)]
        timeout: u64,
    },
    /// Decision log / recent events.
    Log {
        #[arg(long)]
        tail: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Manage offline rules.
    Rules {
        #[command(subcommand)]
        action: RulesAction,
    },
    /// Cluster the decision log into proposed rules.
    BakeBack {
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        apply: Option<String>,
        #[arg(long)]
        reject: Option<String>,
    },
    /// Manage workflow graphs.
    Dag {
        #[command(subcommand)]
        action: DagAction,
    },
    /// Start the loopback HTTP API (default port 4198).
    Api,
    /// The ratatui dashboard (attaches to the running daemon).
    Dashboard,
    /// Open the web UI in the browser (token via the URL hash, in-memory only).
    Web,
    /// Install the launchd user agent (macOS auto-start) for the daemon.
    Install,
    /// Install the supervisor agent (C13) assets into `~/.supervisor/agent/`.
    AgentInstall,
    /// Register a project and generate its `supervisor.toml`.
    Add { path: std::path::PathBuf },
    /// Open a pane attached to a background agent's session.
    Attach { ws: String, agent: String },
    /// List foreground + background agents / attach status.
    Agents {
        #[arg(long)]
        background: bool,
    },
    /// Post an ingested item into the bug/feature intake.
    Ingest {
        /// `github` | `app-feedback` | `cli`.
        source: String,
        /// JSON payload.
        payload: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RulesAction {
    List,
    Reload,
}

#[derive(Debug, Subcommand)]
pub enum DagAction {
    List,
    /// Install a graph JSON file.
    Apply {
        file: std::path::PathBuf,
    },
    Status {
        id: Option<String>,
    },
}

impl Cli {
    /// The API client config derived from flags + state dir.
    fn client_config(&self) -> Result<ClientConfig> {
        ClientConfig::discover(self.state_dir.as_deref())
    }

    /// A connected API client, or a `daemon unreachable` exit.
    fn client(&self) -> Result<ApiClient, ExitCode> {
        let Ok(config) = self.client_config() else {
            eprintln!("supervisor: cannot resolve the daemon config");
            return Err(exit_unreachable());
        };
        ApiClient::new(config).map_err(|_| exit_unreachable())
    }
}

/// Exit code 3: daemon unreachable.
fn exit_unreachable() -> ExitCode {
    ExitCode::from(3)
}

fn main() -> ExitCode {
    // C-5: clap's default usage-error exit is 2; the documented contract
    // (§4.15) says usage errors exit 1.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(1);
        }
    };
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("supervisor: {e:#}");
            ExitCode::from(exit_code(&e))
        }
    }
}

/// Map an error to the documented exit code (§4.15): 1 general/usage,
/// 2 target not found (API 404), 3 daemon unreachable (connect failure).
/// The CLI's slash commands branch on these to self-heal (review C-5).
fn exit_code(err: &anyhow::Error) -> u8 {
    for cause in err.chain() {
        if let Some(f) = cause.downcast_ref::<crate::client::ApiFailure>() {
            return if f.status == 404 { 2 } else { 1 };
        }
        if cause.downcast_ref::<crate::client::DaemonUnreachable>().is_some() {
            return 3;
        }
        if cause.downcast_ref::<crate::client::TargetNotFound>().is_some() {
            return 2;
        }
    }
    1
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Daemon => spawn_daemon(),
        Command::Status => status(cli),
        Command::On { project } => on(cli, project),
        Command::Off { project, force } => off(cli, project, *force),
        Command::Resume => resume(cli),
        Command::Stop => stop(cli),
        Command::Start { ws, graph, vars } => start(cli, ws, graph, vars),
        Command::Smoke { ws, graph, timeout } => smoke(cli, ws, graph, *timeout),
        Command::Log { tail, json } => log(cli, *tail, *json),
        Command::Rules { action } => rules(cli, action),
        Command::BakeBack { preview, apply, reject } => {
            bake_back(cli, *preview, apply.as_deref(), reject.as_deref())
        }
        Command::Dag { action } => dag(cli, action),
        Command::Api => start_api(cli),
        Command::Dashboard => dashboard::run(cli.client_config()?),
        Command::Web => web(cli),
        Command::Install => install_launchd(cli),
        Command::AgentInstall => agent_install(cli),
        Command::Add { path } => add(cli, path.as_path()),
        Command::Attach { ws, agent } => attach(cli, ws, agent),
        Command::Agents { background } => agents(cli, *background),
        Command::Ingest { source, payload } => ingest(cli, source, payload),
    }
}

fn spawn_daemon() -> Result<()> {
    let daemon_bin =
        std::env::var("SUPERVISOR_DAEMON_BIN").unwrap_or_else(|_| "supervisor-daemon".to_owned());
    let status = std::process::Command::new(&daemon_bin).status()?;
    std::process::exit(status.code().unwrap_or(1));
}

fn status(cli: &Cli) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    let _health = client.health()?;
    let workspaces = client.workspaces()?;
    println!("{:12} {:8} {:>6} {:12}", "WORKSPACE", "STATE", "PORT", "UPDATED");
    for ws in &workspaces {
        println!(
            "{:12} {:8} {:>6} {:12}",
            ws["id"].as_str().unwrap_or_default(),
            ws["state"].as_str().unwrap_or_default(),
            ws["port"].as_u64().map_or("-".to_owned(), |p| p.to_string()),
            ws["updated_at"].as_str().unwrap_or_default(),
        );
    }
    for ws in &workspaces {
        let id = ws["id"].as_str().unwrap_or_default();
        if let Ok(agents) = client.agents(id) {
            for agent in &agents {
                // I-21: queue depth in the status output.
                println!(
                    "  {:20} role={:12} state={:10} queued={} session={}",
                    agent["agent_id"].as_str().unwrap_or_default(),
                    agent["role"].as_str().unwrap_or_default(),
                    agent["state"].as_str().unwrap_or_default(),
                    agent["inbox_depth"].as_u64().unwrap_or(0),
                    agent["session_id"].as_str().unwrap_or("none"),
                );
            }
        }
    }
    Ok(())
}

fn on(cli: &Cli, project: &str) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    let result = client.workspace_on(project)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn off(cli: &Cli, project: &str, force: bool) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    let result = client.workspace_off(project, !force)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn resume(cli: &Cli) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    let result = client.resume()?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// Gracefully stop the daemon: read its PID file, send SIGTERM, wait for the
/// process to exit, and report. No pgrep, no guessing — the daemon writes
/// `~/.supervisor/supervisor.pid` on start.
fn stop(cli: &Cli) -> Result<()> {
    let state_dir = cli.state_dir.clone().unwrap_or_else(default_state_dir);
    let pid_path = state_dir.join("supervisor.pid");
    let pid: u32 = std::fs::read_to_string(&pid_path)
        .with_context(|| format!("daemon not running (no pid file at {})", pid_path.display()))?
        .trim()
        .parse()
        .context("corrupt supervisor.pid")?;

    // Is the recorded process actually a live supervisor-daemon?
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !alive {
        anyhow::bail!("daemon not running (pid {pid} is gone; stale pid file)");
    }
    // I-12: a recycled PID (or a planted file) must not be SIGTERMed. Verify
    // the process identity before signaling.
    let identity = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    if !identity.trim().contains("supervisor-daemon") {
        anyhow::bail!(
            "pid {pid} is not a supervisor-daemon process ({identity:?}); refusing to signal"
        );
    }

    println!("stopping supervisor daemon (pid {pid})…");
    let sent = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .context("cannot send SIGTERM")?;
    if !sent.success() {
        anyhow::bail!("failed to signal the daemon");
    }

    // Wait for the graceful shutdown (drain window + children close) to
    // finish. Poll `kill -0` — the daemon exits 0 once cleanup completes.
    // 60s headroom: a stop that arrives during a slow startup (health wait)
    // only reaches the shutdown path after startup completes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    loop {
        let still_alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !still_alive {
            println!("supervisor daemon stopped (pid {pid})");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("daemon did not stop within 30s (check ~/.supervisor/daemon.log)");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// The default state dir, mirroring the daemon's resolution:
/// `SUPERVISOR_STATE_DIR`, else `$HOME/.supervisor`.
fn default_state_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("SUPERVISOR_STATE_DIR") {
        return std::path::PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    std::path::PathBuf::from(home).join(".supervisor")
}

/// Parse a `key=value` workflow variable (F3).
fn parse_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| format!("expected key=value, got {s:?}"))
}

/// Start a workflow graph for a workspace (F3).
fn start(cli: &Cli, ws: &str, graph: &str, vars: &[(String, String)]) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    let vars = vars.iter().cloned().collect::<std::collections::BTreeMap<_, _>>();
    let result = client.start_graph(ws, graph, &vars)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// Live acceptance smoke: drive one workflow node through the whole chain and
/// report each observable hop (Phase A acceptance test).
fn smoke(cli: &Cli, ws: &str, graph: &str, timeout: u64) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;

    println!("smoke: {ws} · {graph}");
    println!("hop 1/5  on: bringing the workspace up");
    let _ = client.workspace_on(ws)?;
    println!("         on: OK");

    println!("hop 2/5  start: starting the graph");
    let start = client.start_graph(ws, graph, &std::collections::BTreeMap::new())?;
    if start.get("already_running").and_then(serde_json::Value::as_bool) == Some(true) {
        // I-11: a re-run would PASS with zero agent work (persisted Done rows
        // + the start no-op) — fail loudly instead.
        anyhow::bail!(
            "smoke: graph {graph} is already live in {ws} — stop it first, or this would false-pass on persisted state"
        );
    }
    println!("         start: OK (root nodes should be Ready)");

    println!("hop 3/5  deliver: waiting for a start message to reach an agent (node → Running)");
    println!("hop 4/5  ack: waiting for the agent's layered ACK (node → Done)");
    println!("hop 5/5  next: waiting for the next node to become Ready");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut saw_running = false;
    let mut saw_done = false;
    loop {
        for node in client.graph_nodes(Some(ws), graph)? {
            let id = node["node_id"].as_str().unwrap_or_default().to_owned();
            let state = node["state"].as_str().unwrap_or_default().to_owned();
            if seen.get(&id) != Some(&state) {
                println!("  node {id:24} → {state}");
                if state == "running" {
                    saw_running = true;
                }
                if state == "done" {
                    saw_done = true;
                }
                seen.insert(id, state);
            }
        }
        let all_done = !seen.is_empty() && seen.values().all(|s| s == "done");
        if all_done && saw_running {
            // I-11: PASS requires observing a live Running hop — "all done"
            // alone can come from persisted rows of a prior run.
            println!("smoke: PASS — the live chain completed end to end");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            println!("smoke: TIMED OUT after {timeout}s — chain stalled");
            println!("  observed: running={saw_running} done={saw_done}");
            if all_done && !saw_running {
                println!(
                    "  note: all nodes are done but no Running hop was observed — likely a re-run over persisted state"
                );
            }
            println!("  last node states:");
            for (id, state) in &seen {
                println!("  node {id:24} → {state}");
            }
            anyhow::bail!("smoke: chain did not complete within {timeout}s");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn log(cli: &Cli, tail: Option<usize>, json: bool) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    let rows = client.decision_log()?;
    let take = tail.unwrap_or(rows.len());
    let rows = rows.iter().rev().take(take).rev().cloned().collect::<Vec<_>>();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for row in rows {
            // M6: the decision column is the tagged action JSON; print a
            // compact summary (action value + outcome when present).
            let action =
                row["decision"].get("kind").and_then(serde_json::Value::as_str).unwrap_or_default();
            let outcome = row["outcome"]
                .as_object()
                .map_or_else(|| "-".to_owned(), |o| serde_json::to_string(o).unwrap_or_default());
            println!(
                "{} {} action={} outcome={}",
                row["ts"].as_str().unwrap_or_default(),
                row["signature"].as_str().unwrap_or_default(),
                action,
                outcome,
            );
        }
    }
    Ok(())
}

fn rules(cli: &Cli, action: &RulesAction) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    match action {
        RulesAction::List => {
            for rule in client.rules()? {
                println!(
                    "{:24} src={:10} conf={:.2} active={}",
                    rule["id"].as_str().unwrap_or_default(),
                    rule["source"].as_str().unwrap_or_default(),
                    rule["confidence"].as_f64().unwrap_or(0.0),
                    rule["active"].as_bool().unwrap_or(false),
                );
            }
        }
        RulesAction::Reload => {
            // The daemon reloads rules from rules.toml on this signal.
            let result = client.reload_rules()?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }
    Ok(())
}

fn bake_back(cli: &Cli, preview: bool, apply: Option<&str>, reject: Option<&str>) -> Result<()> {
    // I-20: a bare `supervisor bake-back` silently did nothing. Require one
    // of the actions.
    if !preview && apply.is_none() && reject.is_none() {
        anyhow::bail!("bake-back requires one of --preview, --apply <id>, or --reject <id>");
    }
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    if let Some(id) = apply {
        let result = client.apply_proposal(id)?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if let Some(id) = reject {
        let result = client.reject_proposal(id)?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if preview {
        // F6: ask the daemon to cluster + generate first, so preview is never
        // empty just because nothing has run yet.
        let preview = client.preview_bakeback()?;
        let created = preview
            .get("created")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        println!("generated {} proposal(s) from the decision log", created.len());
        let proposals = client.proposals()?;
        for p in &proposals {
            println!(
                "{} cluster={} conf={:.2} status={}",
                p["id"].as_str().unwrap_or_default(),
                p["cluster_size"].as_u64().unwrap_or(0),
                p["confidence"].as_f64().unwrap_or(0.0),
                p["status"].as_str().unwrap_or_default(),
            );
            if let Some(toml) = p["rule_toml"].as_str() {
                println!("{toml}");
            }
        }
        if proposals.is_empty() {
            println!("no proposals meet the minimum occurrence threshold");
        }
    }
    Ok(())
}

fn dag(cli: &Cli, action: &DagAction) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    match action {
        DagAction::List => {
            for graph in client.graphs()? {
                println!(
                    "{:24} active={}",
                    graph["id"].as_str().unwrap_or_default(),
                    graph["active"].as_bool().unwrap_or(false),
                );
            }
        }
        DagAction::Apply { file } => {
            let contents = std::fs::read_to_string(file)?;
            let parsed = supervisor_core::dag::Workflow::parse_json(&contents)?;
            let result = client.put_graph(&parsed.graph().id, &contents)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        DagAction::Status { id } => {
            let graphs = client.graphs()?;
            let mut found = id.is_none();
            for graph in graphs {
                let gid = graph["id"].as_str().unwrap_or_default();
                if id.as_deref().is_some_and(|want| want != gid) {
                    continue;
                }
                found = true;
                println!("graph {gid}");
                if let Ok(nodes) = client.graph_nodes(None, gid) {
                    for node in &nodes {
                        // I-1: rows are workspace-scoped; surface the ws.
                        println!(
                            "  {:16} {:24} state={:16} attempt={}",
                            node["workspace_id"].as_str().unwrap_or_default(),
                            node["node_id"].as_str().unwrap_or_default(),
                            node["state"].as_str().unwrap_or_default(),
                            node["attempt"].as_u64().unwrap_or(0),
                        );
                    }
                }
            }
            // I-20: an unknown graph id must not exit 0 silently.
            if !found && let Some(want) = id {
                anyhow::bail!(crate::client::TargetNotFound(format!("unknown graph {want}")));
            }
        }
    }
    Ok(())
}

fn start_api(cli: &Cli) -> Result<()> {
    let _ = cli;
    // The API runs inside the daemon; `supervisor api` ensures it is listening.
    spawn_daemon()
}

/// Open the web UI in the browser with the token in the URL hash (§2.3). The
/// token stays in memory (never persisted); the SPA strips it from the URL.
fn web(cli: &Cli) -> Result<()> {
    let config = cli.client_config()?;
    let url = format!("{}/ui/#token={}", config.base, config.token);
    let opened = std::process::Command::new("open").arg(&url).status().is_ok_and(|s| s.success());
    // I-33: never print the bearer token to the terminal (scrollback, screen
    // recording, wrapper logs). Show the base URL only.
    let display = format!("{}/ui/", config.base);
    if opened {
        println!("opened the supervisor web UI at {display}");
    } else {
        println!("open the UI at:\n{display}");
    }
    Ok(())
}

/// Install the launchd user agent (§5). Requires `supervisor-daemon` on PATH
/// (or `SUPERVISOR_DAEMON_BIN`).
fn install_launchd(cli: &Cli) -> Result<()> {
    let daemon_bin =
        std::env::var("SUPERVISOR_DAEMON_BIN").unwrap_or_else(|_| "supervisor-daemon".to_owned());
    // I-13: honor --state-dir / SUPERVISOR_STATE_DIR consistently, and pass
    // the override through to the plist so the daemon it launches uses the
    // same state dir as the CLI.
    let state_dir = cli.state_dir.clone().unwrap_or_else(default_state_dir);
    let env_override = std::env::var_os("SUPERVISOR_STATE_DIR");
    supervisor_daemon::launchd::install(
        &daemon_bin,
        &state_dir,
        supervisor_daemon::launchd::DEFAULT_LABEL,
        env_override.as_deref().map(std::path::Path::new),
    )?;
    println!(
        "installed launchd agent {label} for {daemon_bin}",
        label = supervisor_daemon::launchd::DEFAULT_LABEL
    );
    Ok(())
}

/// Write the supervisor agent (C13) assets.
fn agent_install(cli: &Cli) -> Result<()> {
    // I-13: resolve the state dir consistently (env override honored).
    let state_dir = cli.state_dir.clone().unwrap_or_else(default_state_dir);
    supervisor_daemon::agent_assets::install(&state_dir)?;
    println!("installed supervisor agent assets under {}/agent", state_dir.display());
    Ok(())
}

fn add(cli: &Cli, path: &std::path::Path) -> Result<()> {
    let name =
        path.file_name().and_then(|s| s.to_str()).ok_or_else(|| anyhow::anyhow!("invalid path"))?;
    let roster = vec![
        supervisor_core::RosterAgent {
            id: "dev_01".to_owned(),
            role: "dev".to_owned(),
            model: None,
            driver: supervisor_core::types::DriverKind::Opencode,
            mode: supervisor_core::types::AgentMode::Foreground,
        },
        supervisor_core::RosterAgent {
            id: "reviewer_01".to_owned(),
            role: "reviewer".to_owned(),
            model: None,
            driver: supervisor_core::types::DriverKind::Opencode,
            mode: supervisor_core::types::AgentMode::Background,
        },
        supervisor_core::RosterAgent {
            id: "tester_01".to_owned(),
            role: "tester".to_owned(),
            model: None,
            driver: supervisor_core::types::DriverKind::Opencode,
            mode: supervisor_core::types::AgentMode::Background,
        },
    ];
    let toml = supervisor_core::config::default_project_toml(
        name,
        &path.to_string_lossy(),
        &roster,
        Some(4101),
    );
    let layout = path.join("supervisor.toml");
    std::fs::write(&layout, toml)?;
    println!("registered {name} at {}", layout.display());
    // Register with the daemon so `supervisor on` can find it immediately.
    if let Ok(client) = cli.client() {
        if let Ok(result) = client.register_workspace(
            name,
            &path.to_string_lossy(),
            Some(&layout.to_string_lossy()),
        ) {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    } else {
        println!(
            "note: daemon not running; the workspace will be discovered on the next daemon start"
        );
    }
    Ok(())
}

fn attach(cli: &Cli, ws: &str, agent: &str) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    let result = client.attach(ws, agent)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn agents(cli: &Cli, background_only: bool) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    println!("{:<16} {:<12} {:<10} {:<12} {:<8}", "WORKSPACE", "AGENT", "MODE", "STATE", "SESSION");
    for ws in client.workspaces()? {
        let ws_id = ws["id"].as_str().unwrap_or_default();
        for agent in client.agents(ws_id)? {
            let mode = agent["mode"].as_str().unwrap_or("foreground");
            if background_only && mode != "background" {
                continue;
            }
            println!(
                "{:16} {:12} {:10} {:12} {}",
                ws_id,
                agent["agent_id"].as_str().unwrap_or_default(),
                mode,
                agent["state"].as_str().unwrap_or_default(),
                agent["session_id"].as_str().unwrap_or("no-session"),
            );
        }
    }
    Ok(())
}

fn ingest(cli: &Cli, source: &str, payload: &str) -> Result<()> {
    let client = cli.client().map_err(|c| anyhow::anyhow!("daemon unreachable ({c:?})"))?;
    let result = client.ingest(source, payload)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
