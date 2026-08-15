//! The loopback HTTP API (C15), phase-2-ready (§4.16).
//!
//! Loopback only (`127.0.0.1:<api_port>`), bearer-token auth from
//! `~/.supervisor/api-token` (generated on first run). Endpoints mirror the
//! CLI: workspace lifecycle, agents, messages, graphs, rules, decision log,
//! bake-back proposals, an SSE event stream, and intake.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use supervisor_core::event::{BusEvent, InboxEvent};
use supervisor_core::types::{InboxEntry, IntakeItem, Priority};
use supervisor_core::{now_rfc3339, time::new_ulid};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::bus::Receiver;
use crate::clients::registry::DriverRegistry;
use crate::services::bakeback::BakebackService;
use crate::services::rules::RuleService;
use crate::services::workflow::WorkflowRunner;
use crate::services::workspace::WorkspaceManager;
use crate::state::Fleet;

/// Shared state for the API.
pub struct AppState {
    pub fleet: Arc<AsyncMutex<Fleet>>,
    pub bus: crate::bus::SharedBus,
    pub workspaces: Arc<WorkspaceManager>,
    pub drivers: Arc<DriverRegistry>,
    pub workflows: Arc<WorkflowRunner>,
    pub rules: Arc<RuleService>,
    pub bakeback: Arc<BakebackService>,
    /// U5: model prices for the cost estimates.
    pub usage_config: supervisor_core::config::RootUsageSection,
    pub token: String,
    /// I-8: the server password, for the documented `Basic opencode:<pw>` auth.
    pub server_password: String,
    pub state_dir: std::path::PathBuf,
    pub shutdown: CancellationToken,
}

/// An error payload.
#[derive(Serialize)]
struct ApiError {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
    }
}

type ApiState = Arc<AppState>;

/// Build the API router plus the static SPA (`/ui/*`, no auth) and the `/`
/// redirect (§2.2). The API routes stay bearer-gated.
#[must_use = "the router does nothing until it is served"]
pub fn router(state: &ApiState) -> Router {
    let state: ApiState = state.clone();
    let api = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/workspaces", get(list_workspaces))
        .route("/api/v1/workspaces", post(register_workspace))
        .route("/api/v1/workspaces/{id}", get(get_workspace))
        .route("/api/v1/workspaces/{id}/on", post(workspace_on))
        .route("/api/v1/workspaces/{id}/off", post(workspace_off))
        .route("/api/v1/resume", post(resume))
        .route("/api/v1/workspaces/{id}/agents", get(list_agents))
        .route("/api/v1/workspaces/{id}/agents/{aid}/message", post(send_message))
        .route("/api/v1/workspaces/{id}/agents/{aid}/messages", get(agent_messages))
        .route("/api/v1/workspaces/{id}/agents/{aid}/permissions/{pid}", post(respond_permission))
        .route("/api/v1/workspaces/{id}/agents/{aid}/abort", post(abort_agent))
        .route("/api/v1/workspaces/{id}/agents/{aid}/attach", post(attach_agent))
        .route("/api/v1/graphs", get(list_graphs))
        .route("/api/v1/graphs/{id}", get(get_graph).put(put_graph).delete(delete_graph))
        .route("/api/v1/graphs/{id}/nodes", get(get_graph_nodes))
        .route("/api/v1/workspaces/{ws}/graphs/{graph}/start", post(start_workflow))
        .route("/api/v1/workspaces/{ws}/graphs/{graph}/nodes/{node}/decide", post(decide_node))
        .route("/api/v1/rules", get(list_rules).post(add_rule))
        .route("/api/v1/rules/reload", post(reload_rules))
        .route("/api/v1/decision-log", get(decision_log))
        .route("/api/v1/triage", get(triage))
        .route("/api/v1/decision-log/{id}/outcome", post(decision_outcome))
        .route("/api/v1/bakeback/proposals", get(list_proposals))
        .route("/api/v1/bakeback/preview", post(preview_bakeback))
        .route("/api/v1/bakeback/proposals/{id}/apply", post(apply_proposal))
        .route("/api/v1/bakeback/proposals/{id}/reject", post(reject_proposal))
        .route("/api/v1/events", get(events))
        .route("/api/v1/ingest", post(ingest))
        .route("/api/v1/intake", get(intake))
        .route("/api/v1/usage", get(usage))
        .route("/api/v1/metrics", get(metrics))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone());

    Router::new()
        .route("/", get(|| async { axum::response::Redirect::to("/ui/") }))
        .route("/ui", get(spa_root))
        .route("/ui/", get(spa_root))
        .route("/ui/{*path}", get(spa))
        .merge(api)
        .with_state(state)
}

/// `/ui` without a trailing slash: serve the SPA root.
async fn spa_root(State(state): State<ApiState>) -> Response {
    spa(State(state), Path(String::new())).await
}

/// Serve the built SPA deterministically: a real file under `~/.supervisor/ui`
/// is served as-is; every other path (client-side routes) falls back to
/// `index.html` (§2.2).
///
/// Security (review C-1): the path is constrained to the UI root. `..`
/// segments and NUL bytes are rejected outright, and the resolved file is
/// canonicalized and required to stay under the canonical UI root — so a
/// request can never serve `api-token`, `secrets.json`, or any other file
/// outside the bundle, even via `--path-as-is` or a symlink escape.
async fn spa(State(state): State<ApiState>, Path(path): Path<String>) -> Response {
    let ui_dir = state.state_dir.join("ui");
    if path.split('/').any(|seg| seg == ".." || seg.contains('\0')) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let raw = if path.is_empty() { ui_dir.join("index.html") } else { ui_dir.join(&path) };
    let file = if raw.is_file() { raw } else { ui_dir.join("index.html") };
    let within_root = match (std::fs::canonicalize(&ui_dir), std::fs::canonicalize(&file)) {
        (Ok(root), Ok(f)) => f.starts_with(&root),
        _ => false,
    };
    if !within_root {
        let message = "supervisor UI is not built yet — run `npm run build` in web/ and copy dist to ~/.supervisor/ui";
        return (StatusCode::NOT_FOUND, message).into_response();
    }
    let Ok(bytes) = tokio::fs::read(&file).await else {
        let message = "supervisor UI is not built yet — run `npm run build` in web/ and copy dist to ~/.supervisor/ui";
        return (StatusCode::NOT_FOUND, message).into_response();
    };
    let mime = mime_for(&file);
    axum::response::Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// A minimal content-type map for the static bundle.
fn mime_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json" | "map") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Bearer-token auth middleware, with the documented `Basic opencode:<pw>`
/// fallback (review I-8 — §4.16 lists basic auth; it was absent).
async fn auth(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let header = request.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let authorized = match header {
        Some(v) if v.starts_with("Bearer ") => {
            v.strip_prefix("Bearer ").is_some_and(|token| token == state.token)
        }
        Some(v) if v.starts_with("Basic ") => {
            // RFC 7617: `Basic base64(user:pass)`. Only the documented
            // `opencode:<server_password>` user is accepted.
            let decoded = v
                .strip_prefix("Basic ")
                .and_then(|b| {
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b).ok()
                })
                .and_then(|b| String::from_utf8(b).ok());
            decoded.is_some_and(|cred| {
                let expected = format!("opencode:{}", state.server_password);
                // Constant-time-ish compare via subtle? Use a plain compare —
                // this is loopback auth, not a network credential boundary.
                cred == expected
            })
        }
        _ => false,
    };
    if authorized {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(ApiError { error: "unauthorized".to_owned() }))
            .into_response()
    }
}

async fn health(State(state): State<ApiState>) -> impl IntoResponse {
    let ws_count = {
        let fleet = state.fleet.lock().await;
        fleet.workspaces().count()
    };
    Json(serde_json::json!({ "healthy": true, "workspaces": ws_count }))
}

async fn list_workspaces(State(state): State<ApiState>) -> Response {
    let fleet = state.fleet.lock().await;
    match serde_json::to_value(fleet.workspaces().cloned().collect::<Vec<_>>()) {
        Ok(value) => Json(value).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

#[derive(Deserialize)]
struct RegisterBody {
    id: String,
    path: String,
    layout_path: Option<String>,
}

/// Register a project as an `off` workspace (`supervisor add` / discovery).
async fn register_workspace(
    State(state): State<ApiState>,
    Json(body): Json<RegisterBody>,
) -> Response {
    let mut fleet = state.fleet.lock().await;
    let existing = fleet.workspace(&body.id).cloned();
    let workspace = supervisor_core::types::Workspace {
        id: body.id.clone(),
        path: body.path,
        port: existing.as_ref().and_then(|w| w.port),
        server_pid: existing.as_ref().and_then(|w| w.server_pid),
        state: supervisor_core::types::WorkspaceState::Off,
        cmux_ws: None,
        layout_path: body.layout_path,
        updated_at: now_rfc3339(),
    };
    match fleet.upsert_workspace(&workspace) {
        Ok(_) => {
            Json(serde_json::json!({ "workspace": body.id, "registered": true })).into_response()
        }
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn get_workspace(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    let fleet = state.fleet.lock().await;
    match fleet.workspace(&id) {
        Some(ws) => match serde_json::to_value(ws) {
            Ok(value) => Json(value).into_response(),
            Err(e) => ApiError { error: e.to_string() }.into_response(),
        },
        None => (StatusCode::NOT_FOUND, Json(ApiError { error: "not found".to_owned() }))
            .into_response(),
    }
}

async fn workspace_on(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    match state.workspaces.on(&id).await {
        Ok(()) => Json(serde_json::json!({ "workspace": id, "state": "on" })).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

#[derive(Deserialize)]
struct OffBody {
    #[serde(default = "default_true")]
    graceful: bool,
}

fn default_true() -> bool {
    true
}

async fn workspace_off(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<OffBody>,
) -> Response {
    match state.workspaces.off(&id, body.graceful).await {
        Ok(()) => Json(serde_json::json!({ "workspace": id, "state": "off" })).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn resume(State(state): State<ApiState>) -> Response {
    match state.workspaces.resume().await {
        Ok(()) => Json(serde_json::json!({ "state": "resumed" })).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn list_agents(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    let fleet = state.fleet.lock().await;
    let agents = fleet.agents(&id).cloned().collect::<Vec<_>>();
    // I-21: surface the per-agent inbox queue depth (§4.15 status requires it).
    let payload: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| {
            let mut value = serde_json::to_value(a).unwrap_or_default();
            value["inbox_depth"] = serde_json::json!(fleet.inbox_depth(&id, &a.agent_id));
            value
        })
        .collect();
    match serde_json::to_value(&payload) {
        Ok(value) => Json(value).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

#[derive(Deserialize)]
struct MessageBody {
    body: String,
    #[serde(default)]
    priority: String,
}

async fn send_message(
    State(state): State<ApiState>,
    Path((ws, agent)): Path<(String, String)>,
    Json(body): Json<MessageBody>,
) -> Response {
    let entry = InboxEntry {
        id: format!("w_{}", new_ulid()),
        workspace_id: ws.clone(),
        agent_id: agent.clone(),
        priority: if body.priority == "high" { Priority::High } else { Priority::Normal },
        body: body.body,
        from: "human".to_owned(),
        kind: "instruction".to_owned(),
        in_reply_to: None,
        ack_for: None,
        delivered: false,
        delivered_at: None,
        created_at: now_rfc3339(),
    };
    let mut fleet = state.fleet.lock().await;
    match fleet.enqueue_inbox(&entry) {
        Ok(record) => {
            // F1: delivery is enqueue-triggered. Without this publish the
            // message waits for the 2s sweep, which only targets agents in
            // the `idle` state — a message to a stuck/spawning agent would
            // sit forever.
            drop(fleet);
            state.bus.publish(BusEvent::Inbox(InboxEvent::Enqueued { entry }));
            Json(serde_json::json!({ "queued": true, "seq": record.seq })).into_response()
        }
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn attach_agent(
    State(state): State<ApiState>,
    Path((ws, agent)): Path<(String, String)>,
) -> Response {
    // M8: spawn a pane attached to a background agent's session (§4.3).
    match state.workspaces.attach_agent(&ws, &agent).await {
        Ok((attach, spawned)) => {
            Json(serde_json::json!({ "attach": attach, "spawned": spawned })).into_response()
        }
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

/// `GET /api/v1/workspaces/{ws}/agents/{aid}/messages?limit=` — transcript for
/// the agent dialog + the usage collector's token source (U3/U5).
async fn agent_messages(
    State(state): State<ApiState>,
    Path((ws, agent)): Path<(String, String)>,
    Query(q): Query<MessagesQuery>,
) -> Response {
    // `?limit=` controls how many transcript rows come back (the previous
    // code read `since` and used it as the limit — a since-timestamp silently
    // became a row count).
    let limit = q.limit.unwrap_or(50).min(200);
    let (driver, agent_ref) = match state.drivers.for_agent(&ws, &agent).await {
        Ok(pair) => pair,
        Err(e) => return ApiError { error: e.to_string() }.into_response(),
    };
    match driver.read_transcript(&agent_ref, limit).await {
        Ok(rows) => match serde_json::to_value(&rows) {
            Ok(value) => Json(value).into_response(),
            Err(e) => ApiError { error: e.to_string() }.into_response(),
        },
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

#[derive(Deserialize)]
struct MessagesQuery {
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct PermissionBody {
    response: String,
    #[serde(default)]
    remember: bool,
}

/// `POST /api/v1/workspaces/{ws}/agents/{aid}/permissions/{pid}`.
async fn respond_permission(
    State(state): State<ApiState>,
    Path((ws, agent, pid)): Path<(String, String, String)>,
    Json(body): Json<PermissionBody>,
) -> Response {
    // A typo'd response must not silently deny (review minor).
    let allow = match body.response.as_str() {
        "allow" => true,
        "deny" => false,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("response must be \"allow\" or \"deny\", got {other:?}"),
                }),
            )
                .into_response();
        }
    };
    let (driver, agent_ref) = match state.drivers.for_agent(&ws, &agent).await {
        Ok(pair) => pair,
        Err(e) => return ApiError { error: e.to_string() }.into_response(),
    };
    match driver.respond_permission(&agent_ref, &pid, allow, body.remember).await {
        Ok(()) => Json(serde_json::json!({ "permission": pid, "response": body.response, "remember": body.remember }))
            .into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

/// `POST /api/v1/workspaces/{ws}/agents/{aid}/abort`.
async fn abort_agent(
    State(state): State<ApiState>,
    Path((ws, agent)): Path<(String, String)>,
) -> Response {
    let (driver, agent_ref) = match state.drivers.for_agent(&ws, &agent).await {
        Ok(pair) => pair,
        Err(e) => return ApiError { error: e.to_string() }.into_response(),
    };
    match driver.abort(&agent_ref).await {
        Ok(()) => Json(serde_json::json!({ "aborted": true })).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn list_graphs(State(state): State<ApiState>) -> Response {
    let fleet = state.fleet.lock().await;
    let graphs = fleet.graphs().cloned().collect::<Vec<_>>();
    match serde_json::to_value(&graphs) {
        Ok(value) => Json(value).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn get_graph(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    let fleet = state.fleet.lock().await;
    match fleet.graph(&id) {
        Some(g) => match serde_json::to_value(g) {
            Ok(value) => Json(value).into_response(),
            Err(e) => ApiError { error: e.to_string() }.into_response(),
        },
        None => (StatusCode::NOT_FOUND, Json(ApiError { error: "graph not found".to_owned() }))
            .into_response(),
    }
}

async fn put_graph(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(data): Json<serde_json::Value>,
) -> Response {
    // Validate by parsing before persisting; any valid graph JSON is accepted
    // (custom graphs install via `supervisor dag apply`). The path id must
    // match the graph JSON's internal id so a `dag apply` round-trip never
    // persists `foo` with data that self-identifies as `bar` (review round 2,
    // finding 4).
    let Some(raw) = data.get("data").and_then(serde_json::Value::as_str) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: "missing graph data".to_owned() }),
        )
            .into_response();
    };
    if let Err(e) = validate_graph_put(&id, raw) {
        return (StatusCode::BAD_REQUEST, Json(ApiError { error: e })).into_response();
    }
    let graph = supervisor_core::types::Graph {
        id: id.clone(),
        name: id.clone(),
        data: raw.to_owned(),
        version: 1,
        active: true,
        updated_at: now_rfc3339(),
    };
    let mut fleet = state.fleet.lock().await;
    match fleet.upsert_graph(&graph) {
        Ok(_) => Json(serde_json::json!({ "graph": id, "saved": true })).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

/// Validate a graph PUT (pure, for tests): the body must be parseable graph
/// JSON and its internal id must equal the path id.
fn validate_graph_put(path_id: &str, raw: &str) -> Result<(), String> {
    let workflow = supervisor_core::dag::Workflow::parse_json(raw)
        .map_err(|e| format!("invalid graph JSON: {e}"))?;
    if workflow.graph().id != path_id {
        return Err(format!(
            "graph id {path_id:?} does not match the graph data's id {:?}",
            workflow.graph().id
        ));
    }
    Ok(())
}

async fn delete_graph(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    // Graphs keep their history; DELETE deactivates the graph (stops it from
    // being offered to new runs) rather than pretending a 200 that deletes
    // nothing (review minor).
    let mut fleet = state.fleet.lock().await;
    let Some(mut graph) = fleet.graph(&id).cloned() else {
        return (StatusCode::NOT_FOUND, Json(ApiError { error: format!("unknown graph {id:?}") }))
            .into_response();
    };
    graph.active = false;
    graph.updated_at = now_rfc3339();
    match fleet.upsert_graph(&graph) {
        Ok(_) => Json(serde_json::json!({ "graph": id, "deleted": true, "deactivated": true }))
            .into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

#[derive(Deserialize)]
struct GraphNodesQuery {
    /// I-1: node state is workspace-scoped; `ws` filters the rows.
    ws: Option<String>,
}

async fn get_graph_nodes(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<GraphNodesQuery>,
) -> Response {
    let fleet = state.fleet.lock().await;
    let rows: Vec<_> = match query.ws.as_deref() {
        Some(ws) => fleet.node_states(ws, &id).cloned().collect(),
        None => fleet.node_states_all(&id).cloned().collect(),
    };
    match serde_json::to_value(&rows) {
        Ok(value) => Json(value).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

#[derive(Deserialize)]
struct StartBody {
    #[serde(default)]
    vars: std::collections::BTreeMap<String, String>,
}

/// `POST /api/v1/workspaces/{ws}/graphs/{graph}/start` (F3): bring the
/// workspace on if off, then start the workflow.
async fn start_workflow(
    State(state): State<ApiState>,
    Path((ws, graph)): Path<(String, String)>,
    Json(body): Json<StartBody>,
) -> Response {
    if let Err(e) = state.workspaces.on(&ws).await {
        return ApiError { error: format!("workspace on failed: {e}") }.into_response();
    }
    match state.workflows.start_graph(&ws, &graph, body.vars).await {
        Ok(started) => {
            // I-11: report when the graph was already live so callers (the
            // smoke) cannot false-pass on a re-run.
            Json(serde_json::json!({
                "started": started,
                "already_running": !started,
                "graph": graph,
                "workspace": ws
            }))
            .into_response()
        }
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

#[derive(Deserialize)]
struct DecideBody {
    /// `done` | `rerun` | `skip`.
    action: String,
    #[serde(default)]
    reason: Option<String>,
}

/// A4: `POST /api/v1/workspaces/{ws}/graphs/{graph}/nodes/{node}/decide` —
/// a human ruling on a `NeedsDecision` node. Journaled as a decision record
/// before the engine transition (C-2).
async fn decide_node(
    State(state): State<ApiState>,
    Path((ws, graph, node)): Path<(String, String, String)>,
    Json(body): Json<DecideBody>,
) -> Response {
    match state.workflows.decide(&ws, &graph, &node, &body.action, body.reason.as_deref()).await {
        Ok(new_state) => Json(serde_json::json!({
            "node": node,
            "state": new_state,
            "action": body.action,
            "workspace": ws,
            "graph": graph,
        }))
        .into_response(),
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("not needs_decision") {
                StatusCode::CONFLICT
            } else if msg.contains("unknown") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(ApiError { error: msg })).into_response()
        }
    }
}

/// A5: `GET /api/v1/triage` — the read-only attention aggregate: agents in
/// `waiting_input` / `blocked_permission` / `error`, and nodes in
/// `needs_decision` / `failed` / `blocked` / `missing_role`. Dumb on purpose:
/// sorting and filtering are client-side.
async fn triage(State(state): State<ApiState>) -> Response {
    let fleet = state.fleet.lock().await;
    let mut agents = Vec::new();
    let mut nodes = Vec::new();
    for ws in fleet.workspaces() {
        for a in fleet.agents(&ws.id) {
            if matches!(
                a.state,
                supervisor_core::types::AgentState::WaitingInput
                    | supervisor_core::types::AgentState::BlockedPermission
                    | supervisor_core::types::AgentState::Error
            ) {
                agents.push(serde_json::json!({
                    "ws": ws.id,
                    "agent_id": a.agent_id,
                    "state": a.state,
                    "permission_id": null,
                }));
            }
        }
        for g in fleet.graphs() {
            for row in fleet.node_states(&ws.id, &g.id) {
                if matches!(
                    row.state,
                    supervisor_core::types::NodeState::NeedsDecision
                        | supervisor_core::types::NodeState::Failed
                        | supervisor_core::types::NodeState::Blocked
                        | supervisor_core::types::NodeState::MissingRole
                ) {
                    nodes.push(serde_json::json!({
                        "ws": ws.id,
                        "graph_id": g.id,
                        "node_id": row.node_id,
                        "state": row.state,
                        "error": row.error,
                    }));
                }
            }
        }
    }
    Json(serde_json::json!({ "agents": agents, "nodes": nodes })).into_response()
}

/// Ensure a workspace is `on` and start the graph for an intake item, linking
/// the intake row. Returns the started graph id (or `None` for non-workflow
/// kinds). The item's fields feed the workflow vars (review finding 2) so
/// `{bug}`/`{feature}`/`{spec}` placeholders render.
async fn start_graph_for_intake(
    state: &ApiState,
    ws: &str,
    item: &IntakeItem,
) -> Result<Option<String>, String> {
    let Some(graph) = item.workflow_graph() else { return Ok(None) };
    state.workspaces.on(ws).await.map_err(|e| format!("workspace on failed: {e}"))?;
    state
        .workflows
        .start_graph(ws, graph, item.workflow_vars())
        .await
        .map_err(|e| e.to_string())?;
    {
        let mut fleet = state.fleet.lock().await;
        if let Err(e) = fleet.link_intake(&item.id, graph) {
            tracing::error!(intake = %item.id, graph, error = %e, "link intake graph failed");
        }
    }
    Ok(Some(graph.to_owned()))
}

/// Reload the data rules from the global `rules.toml` (hot reload, §4.10).
async fn reload_rules(State(state): State<ApiState>) -> Response {
    let path = state.state_dir.join("rules.toml");
    match std::fs::read_to_string(&path) {
        Ok(contents) => match state.rules.reload(&contents) {
            Ok(()) => Json(serde_json::json!({ "reloaded": true })).into_response(),
            Err(e) => ApiError { error: e.to_string() }.into_response(),
        },
        Err(e) => {
            ApiError { error: format!("cannot read {}: {e}", path.display()) }.into_response()
        }
    }
}

async fn list_rules(State(state): State<ApiState>) -> Response {
    let fleet = state.fleet.lock().await;
    match serde_json::to_value(fleet.rules().cloned().collect::<Vec<_>>()) {
        Ok(value) => Json(value).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn add_rule(State(state): State<ApiState>, Json(body): Json<serde_json::Value>) -> Response {
    let toml = body.get("toml").and_then(serde_json::Value::as_str);
    let Some(toml) = toml else {
        return (StatusCode::BAD_REQUEST, Json(ApiError { error: "missing toml".to_owned() }))
            .into_response();
    };
    match supervisor_core::rules::Rule::parse_toml(toml) {
        Ok(rules) if !rules.is_empty() => {
            let rule = supervisor_core::types::StoredRule {
                id: rules[0].id.clone(),
                toml: toml.to_owned(),
                source: "data".to_owned(),
                confidence: rules[0].confidence,
                approved: true,
                active: true,
                created_at: now_rfc3339(),
            };
            let mut fleet = state.fleet.lock().await;
            match fleet.upsert_rule(&rule) {
                Ok(_) => {
                    if let Err(e) = state.rules.reload_rules_from(&rule.toml) {
                        tracing::error!(error = %e, "hot-reload after rule add failed");
                    }
                    Json(serde_json::json!({ "rule": rule.id, "added": true })).into_response()
                }
                Err(e) => ApiError { error: e.to_string() }.into_response(),
            }
        }
        _ => (StatusCode::BAD_REQUEST, Json(ApiError { error: "invalid rule toml".to_owned() }))
            .into_response(),
    }
}

#[derive(Deserialize)]
struct SinceQuery {
    since: Option<String>,
}

async fn decision_log(State(state): State<ApiState>, Query(q): Query<SinceQuery>) -> Response {
    let fleet = state.fleet.lock().await;
    let since = q.since.as_deref().unwrap_or("1970-01-01T00:00:00.000Z");
    let rows =
        fleet.decisions().iter().filter(|d| d.ts.as_str() >= since).cloned().collect::<Vec<_>>();
    match serde_json::to_value(&rows) {
        Ok(value) => Json(value).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

/// F6: generate proposals from the decision log, then list them (newly created
/// + pending, for display).
async fn preview_bakeback(State(state): State<ApiState>) -> Response {
    let created = match state.bakeback.preview().await {
        Ok(created) => created,
        Err(e) => return ApiError { error: e.to_string() }.into_response(),
    };
    let pending = match state.bakeback.pending().await {
        Ok(pending) => pending,
        Err(e) => return ApiError { error: e.to_string() }.into_response(),
    };
    Json(serde_json::json!({
        "created": created,
        "pending": pending,
        "created_count": created.len(),
        "pending_count": pending.len(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct OutcomeBody {
    /// `applied` | `failed`.
    result: String,
    note: Option<String>,
}

/// M10: record a decision outcome so bake-back sees real results.
async fn decision_outcome(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<OutcomeBody>,
) -> Response {
    let success = body.result == "applied";
    let outcome = serde_json::json!({
        "result": body.result,
        "success": success,
        "note": body.note,
    });
    let mut fleet = state.fleet.lock().await;
    match fleet.record_decision_outcome(&id, &outcome) {
        Ok(()) => Json(serde_json::json!({ "decision": id, "outcome": outcome })).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn list_proposals(State(state): State<ApiState>) -> Response {
    let fleet = state.fleet.lock().await;
    match serde_json::to_value(fleet.proposals().cloned().collect::<Vec<_>>()) {
        Ok(value) => Json(value).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn apply_proposal(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    match state.bakeback.apply(&id).await {
        Ok(true) => {
            Json(serde_json::json!({ "proposal": id, "status": "applied" })).into_response()
        }
        Ok(false) => Json(serde_json::json!({ "proposal": id, "status": "no-op" })).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

async fn reject_proposal(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    match state.bakeback.reject(&id).await {
        Ok(true) => {
            Json(serde_json::json!({ "proposal": id, "status": "rejected" })).into_response()
        }
        Ok(false) => Json(serde_json::json!({ "proposal": id, "status": "no-op" })).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

/// SSE stream of internal bus events.
async fn events(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = std::result::Result<SseEvent, std::convert::Infallible>>> {
    let mut rx: Receiver = state.bus.subscribe();
    let stream = async_stream::stream! {
        while let Ok(event) = rx.recv().await {
            let payload = serde_json::to_string(&event).unwrap_or_default();
            yield Ok(SseEvent::default().data(payload));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct IngestBody {
    source: String,
    payload: serde_json::Value,
}

async fn ingest(State(state): State<ApiState>, Json(body): Json<IngestBody>) -> Response {
    let kind = body
        .payload
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("feedback")
        .to_owned();
    let title = body
        .payload
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let text =
        body.payload.get("body").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let item = IntakeItem {
        id: format!("in_{}", new_ulid()),
        source: body.source,
        kind: kind.clone(),
        title,
        body: text,
        severity: body
            .payload
            .get("severity")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        refs: Vec::new(),
        graph_id: None,
        received_at: now_rfc3339(),
    };
    let ws = body.payload.get("workspace").and_then(serde_json::Value::as_str).map(str::to_owned);
    {
        let mut fleet = state.fleet.lock().await;
        if let Err(e) = fleet.insert_intake(&item) {
            return ApiError { error: e.to_string() }.into_response();
        }
    }
    // F3: drive the intake into a workflow (bug-from-off brings the workspace
    // on first). A product that posts without a `workspace` gets intake only.
    let workflow = match ws {
        Some(ws) => match start_graph_for_intake(&state, &ws, &item).await {
            Ok(graph) => graph.unwrap_or_else(|| "none".to_owned()),
            Err(e) => return ApiError { error: e }.into_response(),
        },
        None => "none".to_owned(),
    };
    Json(serde_json::json!({ "intake": item.id, "queued": true, "workflow": workflow }))
        .into_response()
}

async fn intake(State(state): State<ApiState>) -> Response {
    let fleet = state.fleet.lock().await;
    match serde_json::to_value(fleet.intake().cloned().collect::<Vec<_>>()) {
        Ok(value) => Json(value).into_response(),
        Err(e) => ApiError { error: e.to_string() }.into_response(),
    }
}

#[derive(Deserialize)]
struct UsageQuery {
    ws: Option<String>,
    agent: Option<String>,
    since: Option<String>,
}

/// `GET /api/v1/usage?ws=&agent=&since=` — usage rows with computed estimated
/// cost (unknown models → `cost_cents: null`, shown as "—").
async fn usage(State(state): State<ApiState>, Query(q): Query<UsageQuery>) -> Response {
    let rows = {
        let fleet = state.fleet.lock().await;
        match fleet.usage_since(q.ws.as_deref(), q.agent.as_deref(), q.since.as_deref()) {
            Ok(rows) => rows,
            Err(e) => return ApiError { error: e.to_string() }.into_response(),
        }
    };
    let costed: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let cost = row.model.as_deref().and_then(|m| {
                state.usage_config.cost_cents(m, row.prompt_tokens, row.completion_tokens)
            });
            serde_json::json!({
                "id": row.id,
                "workspace_id": row.workspace_id,
                "agent_id": row.agent_id,
                "model": row.model,
                "ts": row.ts,
                "prompt_tokens": row.prompt_tokens,
                "completion_tokens": row.completion_tokens,
                "cost_cents": cost,
            })
        })
        .collect();
    Json(serde_json::json!({ "rows": costed, "count": costed.len() })).into_response()
}

#[derive(Deserialize)]
struct MetricsQuery {
    since: Option<String>,
    bucket: Option<String>,
}

/// `GET /api/v1/metrics?since=` — aggregated dashboard numbers (§3.4). Cost is
/// estimated from `model_prices`; unknown models contribute tokens only.
async fn metrics(State(state): State<ApiState>, Query(q): Query<MetricsQuery>) -> Response {
    let since = q.since.as_deref().unwrap_or("1970-01-01T00:00:00.000Z");
    let fleet = state.fleet.lock().await;

    let decisions =
        fleet.decisions().iter().filter(|d| d.ts.as_str() >= since).cloned().collect::<Vec<_>>();
    let usage_rows = match fleet.usage_since(None, None, Some(since)) {
        Ok(rows) => rows,
        Err(e) => return ApiError { error: e.to_string() }.into_response(),
    };

    let sum = |rows: &[supervisor_core::types::UsageRow]| -> (u64, u64, Option<f64>) {
        let prompt: u64 = rows.iter().map(|r| r.prompt_tokens).sum();
        let completion: u64 = rows.iter().map(|r| r.completion_tokens).sum();
        let mut cost = Some(0.0);
        for row in rows {
            if let Some(c) = row.model.as_deref().and_then(|m| {
                state.usage_config.cost_cents(m, row.prompt_tokens, row.completion_tokens)
            }) {
                cost = cost.map(|acc| acc + c);
            }
        }
        (prompt + completion, completion, cost)
    };

    let (tokens, _completion, cost) = sum(&usage_rows);
    let (nodes_done, nodes_failed, delivered, errors) = fleet.aggregate_counts();
    let totals = serde_json::json!({
        "messages_delivered": delivered,
        "errors": errors,
        "decisions": decisions.len(),
        "nodes_done": nodes_done,
        "nodes_failed": nodes_failed,
        "tokens": tokens,
        "cost_cents": cost,
    });

    let mut per_workspace: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    for row in &usage_rows {
        let entry = per_workspace
            .entry(row.workspace_id.clone())
            .or_insert_with(|| serde_json::json!({"decisions": 0, "tokens": 0, "cost_cents": 0.0}));
        entry["tokens"] = serde_json::json!(
            entry["tokens"].as_u64().unwrap_or(0) + row.prompt_tokens + row.completion_tokens
        );
        if let Some(c) = row.model.as_deref().and_then(|m| {
            state.usage_config.cost_cents(m, row.prompt_tokens, row.completion_tokens)
        }) {
            entry["cost_cents"] =
                serde_json::json!(entry["cost_cents"].as_f64().unwrap_or(0.0) + c);
        }
    }
    for d in &decisions {
        // Decisions carry the ws in their situation JSON.
        if let Some(ws) = d.situation.get("ws").and_then(serde_json::Value::as_str) {
            let entry = per_workspace.entry(ws.to_owned()).or_insert_with(
                || serde_json::json!({"decisions": 0, "tokens": 0, "cost_cents": 0.0}),
            );
            entry["decisions"] = serde_json::json!(entry["decisions"].as_u64().unwrap_or(0) + 1);
        }
    }

    // time_series bucketed by hour (or day).
    let bucket = if q.bucket.as_deref() == Some("1d") { 86_400u64 } else { 3_600u64 };
    let mut series: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    let bucket_key = |ts: &str| -> String {
        // Lexicographic on RFC 3339 works; bucket on the first N chars.
        let key = ts.to_owned();
        let prefix_len = if bucket == 86_400 { 10 } else { 13 };
        key.chars().take(prefix_len).collect()
    };
    for row in &usage_rows {
        let entry = series
            .entry(bucket_key(&row.ts))
            .or_insert_with(|| serde_json::json!({"messages": 0, "errors": 0, "cost_cents": 0.0}));
        entry["messages"] = serde_json::json!(entry["messages"].as_u64().unwrap_or(0) + 1);
        if let Some(c) = row.model.as_deref().and_then(|m| {
            state.usage_config.cost_cents(m, row.prompt_tokens, row.completion_tokens)
        }) {
            entry["cost_cents"] =
                serde_json::json!(entry["cost_cents"].as_f64().unwrap_or(0.0) + c);
        }
    }
    let time_series: Vec<serde_json::Value> = series
        .into_iter()
        .map(|(ts, mut v)| {
            v["ts"] = serde_json::json!(ts);
            v
        })
        .collect();

    Json(serde_json::json!({
        "since": since,
        "totals": totals,
        "per_workspace": per_workspace,
        "per_agent": {},
        "time_series": time_series,
    }))
    .into_response()
}

/// Load or create the API token file.
///
/// # Errors
/// Any I/O failure.
pub fn load_or_create_token(path: &std::path::Path) -> Result<String> {
    use base64::Engine as _;
    use rand::RngCore as _;
    if path.exists() {
        return std::fs::read_to_string(path)
            .map(|s| s.trim().to_owned())
            .with_context(|| format!("reading {}", path.display()));
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    crate::secrets::write_secure(token.as_bytes(), path)?;
    Ok(token)
}

/// Build the API token path under a state dir.
#[must_use]
pub fn token_path(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("api-token")
}

#[cfg(test)]
mod tests {
    use super::validate_graph_put;

    const GRAPH: &str = r#"{"id":"g","name":"g","nodes":[
        {"id":"n","role":"dev","start_template":"x","done_when":{"ack":"n"}}
    ]}"#;

    #[test]
    fn put_graph_accepts_a_matching_id() {
        assert!(validate_graph_put("g", GRAPH).is_ok());
    }

    #[test]
    fn put_graph_rejects_a_path_id_mismatch() {
        // Review round 2, finding 4: a PUT /graphs/foo whose data self-
        // identifies as `bar` would persist `foo` with mismatched data.
        assert!(validate_graph_put("foo", GRAPH).is_err());
    }

    #[test]
    fn put_graph_rejects_invalid_json() {
        assert!(validate_graph_put("g", "not json").is_err());
    }
}
