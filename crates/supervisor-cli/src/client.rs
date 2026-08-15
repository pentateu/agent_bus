//! The CLI's thin HTTP client over the daemon's loopback API (§4.16).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};

/// Resolved client config from the state dir.
pub struct ClientConfig {
    pub base: String,
    pub token: String,
}

impl ClientConfig {
    /// Discover the daemon endpoint + token from the state dir.
    ///
    /// # Errors
    /// Missing state dir or token.
    pub fn discover(state_dir: Option<&Path>) -> Result<Self> {
        let dir = state_dir.map_or_else(default_state_dir, Path::to_owned);
        let api_port = read_api_port(&dir);
        let token_path = dir.join("api-token");
        let token = std::fs::read_to_string(&token_path)
            .context("API token not found; is the daemon running?")?
            .trim()
            .to_owned();
        Ok(Self { base: format!("http://127.0.0.1:{api_port}"), token })
    }
}

/// The default state dir: `SUPERVISOR_STATE_DIR`, else `$HOME/.supervisor`.
/// The env override makes the state dir deterministic even when a sandboxed
/// shell sets HOME to a temp dir (caught live: the daemon and CLI resolved
/// different state dirs, so every command 401'd).
fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SUPERVISOR_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".supervisor")
}

/// Read `api_port` from the root config, defaulting to 4198.
fn read_api_port(dir: &Path) -> u16 {
    let path = dir.join("supervisor.toml");
    let Ok(contents) = std::fs::read_to_string(&path) else { return 4198 };
    let Ok(config) = supervisor_core::config::SupervisorConfig::parse(&contents) else {
        return 4198;
    };
    config.supervisor.api_port
}

/// The API client.
pub struct ApiClient {
    http: Client,
    base: String,
    token: String,
}

impl ApiClient {
    /// Build a client. Uses a generous timeout because graceful `off` can wait
    /// for in-flight turns (up to the graceful window).
    ///
    /// # Errors
    /// Client construction failure.
    pub fn new(config: ClientConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_mins(3))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, base: config.base, token: config.token })
    }

    fn get(&self, path: &str) -> Result<serde_json::Value> {
        let url = format!("{}{path}", self.base);
        let res = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| map_send_err(&e, "GET {path}"))?;
        parse(res)
    }

    fn post(&self, path: &str, body: Option<&serde_json::Value>) -> Result<serde_json::Value> {
        let url = format!("{}{path}", self.base);
        let mut req = self.http.post(&url).bearer_auth(&self.token);
        if let Some(body) = body {
            req = req.json(body);
        }
        parse(req.send().map_err(|e| map_send_err(&e, "POST {path}"))?)
    }

    fn put(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}{path}", self.base);
        let req = self.http.put(&url).bearer_auth(&self.token).json(body);
        parse(req.send().map_err(|e| map_send_err(&e, "PUT {path}"))?)
    }

    pub fn health(&self) -> Result<serde_json::Value> {
        self.get("/api/v1/health")
    }

    pub fn workspaces(&self) -> Result<Vec<serde_json::Value>> {
        let value = self.get("/api/v1/workspaces")?;
        serde_json::from_value(value).context("decode workspaces")
    }

    pub fn agents(&self, ws: &str) -> Result<Vec<serde_json::Value>> {
        let value = self.get(&format!("/api/v1/workspaces/{ws}/agents"))?;
        serde_json::from_value(value).context("decode agents")
    }

    pub fn workspace_on(&self, ws: &str) -> Result<serde_json::Value> {
        self.post(&format!("/api/v1/workspaces/{ws}/on"), None)
    }

    /// Register a project with the daemon (`supervisor add`).
    pub fn register_workspace(
        &self,
        id: &str,
        path: &str,
        layout_path: Option<&str>,
    ) -> Result<serde_json::Value> {
        self.post(
            "/api/v1/workspaces",
            Some(&serde_json::json!({ "id": id, "path": path, "layout_path": layout_path })),
        )
    }

    pub fn workspace_off(&self, ws: &str, graceful: bool) -> Result<serde_json::Value> {
        self.post(
            &format!("/api/v1/workspaces/{ws}/off"),
            Some(&serde_json::json!({ "graceful": graceful })),
        )
    }

    pub fn resume(&self) -> Result<serde_json::Value> {
        self.post("/api/v1/resume", None)
    }

    /// Start a workflow graph for a workspace (F3).
    pub fn start_graph(
        &self,
        ws: &str,
        graph: &str,
        vars: &std::collections::BTreeMap<String, String>,
    ) -> Result<serde_json::Value> {
        self.post(
            &format!("/api/v1/workspaces/{ws}/graphs/{graph}/start"),
            Some(&serde_json::json!({ "vars": vars })),
        )
    }

    pub fn decision_log(&self) -> Result<Vec<serde_json::Value>> {
        let value = self.get("/api/v1/decision-log")?;
        serde_json::from_value(value).context("decode decision log")
    }

    pub fn rules(&self) -> Result<Vec<serde_json::Value>> {
        let value = self.get("/api/v1/rules")?;
        serde_json::from_value(value).context("decode rules")
    }

    pub fn reload_rules(&self) -> Result<serde_json::Value> {
        self.post("/api/v1/rules/reload", None)
    }

    pub fn proposals(&self) -> Result<Vec<serde_json::Value>> {
        let value = self.get("/api/v1/bakeback/proposals")?;
        serde_json::from_value(value).context("decode proposals")
    }

    /// F6: trigger proposal generation on the daemon, then list them.
    pub fn preview_bakeback(&self) -> Result<serde_json::Value> {
        self.post("/api/v1/bakeback/preview", None)
    }

    pub fn apply_proposal(&self, id: &str) -> Result<serde_json::Value> {
        self.post(&format!("/api/v1/bakeback/proposals/{id}/apply"), None)
    }

    pub fn reject_proposal(&self, id: &str) -> Result<serde_json::Value> {
        self.post(&format!("/api/v1/bakeback/proposals/{id}/reject"), None)
    }

    pub fn graphs(&self) -> Result<Vec<serde_json::Value>> {
        let value = self.get("/api/v1/graphs")?;
        serde_json::from_value(value).context("decode graphs")
    }

    pub fn put_graph(&self, id: &str, data: &str) -> Result<serde_json::Value> {
        // M5: the daemon route is PUT-only; POSTing returned a 405 on every
        // graph save.
        self.put(&format!("/api/v1/graphs/{id}"), &serde_json::json!({ "data": data }))
    }

    pub fn graph_nodes(&self, ws: Option<&str>, id: &str) -> Result<Vec<serde_json::Value>> {
        // I-1: node state is workspace-scoped; `ws` filters, absent = all.
        let path = match ws {
            Some(ws) => format!("/api/v1/graphs/{id}/nodes?ws={ws}"),
            None => format!("/api/v1/graphs/{id}/nodes"),
        };
        let value = self.get(&path)?;
        serde_json::from_value(value).context("decode graph nodes")
    }

    pub fn attach(&self, ws: &str, agent: &str) -> Result<serde_json::Value> {
        self.post(&format!("/api/v1/workspaces/{ws}/agents/{agent}/attach"), None)
    }

    pub fn ingest(&self, source: &str, payload: &str) -> Result<serde_json::Value> {
        // I-19: propagate a bad payload instead of silently creating an empty
        // intake row (previously `unwrap_or_default` reported success on a
        // typo'd payload).
        let payload: serde_json::Value = serde_json::from_str(payload)
            .with_context(|| format!("ingest payload is not valid JSON: {payload}"))?;
        self.post(
            "/api/v1/ingest",
            Some(&serde_json::json!({ "source": source, "payload": payload })),
        )
    }
}

/// Carries an HTTP status so `main` can map it to the documented exit codes
/// (§4.15): a 404 → exit 2 (target not found). Review C-5.
#[derive(Debug)]
pub struct ApiFailure {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "API returned {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiFailure {}

/// A connect-level failure (the daemon is not reachable) → exit 3. Review C-5.
#[derive(Debug)]
pub struct DaemonUnreachable(pub String);

impl std::fmt::Display for DaemonUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "daemon unreachable: {}", self.0)
    }
}

impl std::error::Error for DaemonUnreachable {}

/// A target (workspace/graph/node) does not exist → exit 2. Review I-20.
#[derive(Debug)]
pub struct TargetNotFound(pub String);

impl std::fmt::Display for TargetNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not found: {}", self.0)
    }
}

impl std::error::Error for TargetNotFound {}

/// Map a reqwest send failure: a connect/timeout/request error means the
/// daemon is unreachable (exit 3); anything else is a general failure.
fn map_send_err(e: &reqwest::Error, what: &str) -> anyhow::Error {
    if e.is_connect() || e.is_timeout() || e.is_request() {
        anyhow::Error::new(DaemonUnreachable(format!("{what}: {e}")))
    } else {
        anyhow::anyhow!("{what}: {e}")
    }
}

/// Parse a response, turning non-2xx into a message. 404s carry the status so
/// the CLI can exit 2 (target not found).
fn parse(res: Response) -> Result<serde_json::Value> {
    if res.status() == StatusCode::UNAUTHORIZED {
        anyhow::bail!("unauthorized: is the API token current?");
    }
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().unwrap_or_default();
        return Err(ApiFailure { status, message: text }.into());
    }
    res.json().context("decode response")
}
