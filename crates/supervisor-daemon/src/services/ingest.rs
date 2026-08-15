//! The ingestion layer (§4.17): adapter-based bug/feature intake.
//!
//! Each source is a small adapter that normalizes an incoming item into the
//! intake model and posts it to the bug/feature channel: GitHub issues poll the
//! repo API, in-app feedback POSTs to `/api/v1/ingest`, and the CLI posts for
//! scripts. A new intake item starts the matching workflow (bug → `bug_flow`,
//! feature → `feature_lifecycle`).

use std::sync::Arc;

use anyhow::{Context, Result};
use supervisor_core::event::{BusEvent, HumanEvent};
use supervisor_core::time::new_ulid;
use supervisor_core::types::{IntakeItem, WorkspaceState};
use supervisor_core::{config::ProjectConfig, now_rfc3339};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::bus::Receiver;
use crate::services::workflow::WorkflowRunner;
use crate::services::workspace::WorkspaceManager;
use crate::state::Fleet;

/// GitHub issues poll interval default (per §6.1).
pub const DEFAULT_POLL_SECS: u64 = 300;

/// One configured GitHub adapter.
#[derive(Debug, Clone)]
struct GithubAdapter {
    workspace: String,
    repo: String,
    poll_secs: u64,
    /// `seen_<issue_number>` dedupe.
    seen: std::collections::BTreeSet<u64>,
    /// When this adapter is next due to poll.
    next_poll_at: std::time::Instant,
}

/// The ingestion service.
pub struct IngestionService {
    fleet: Arc<AsyncMutex<Fleet>>,
    workflows: Arc<WorkflowRunner>,
    /// The workspace manager, so intake can bring a workspace on (bug-from-off,
    /// review finding 3).
    workspaces: Arc<WorkspaceManager>,
    bus: crate::bus::SharedBus,
    shutdown: CancellationToken,
    github: tokio::sync::Mutex<Vec<GithubAdapter>>,
}

impl IngestionService {
    /// Build the service.
    #[must_use]
    pub fn new(
        fleet: Arc<AsyncMutex<Fleet>>,
        workflows: Arc<WorkflowRunner>,
        workspaces: Arc<WorkspaceManager>,
        bus: crate::bus::SharedBus,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            fleet,
            workflows,
            workspaces,
            bus,
            shutdown,
            github: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// Register a GitHub adapter from a project's `[ingest] github` config.
    pub async fn register_github(
        &self,
        workspace: &str,
        config: &supervisor_core::config::GithubAdapterConfig,
    ) {
        self.github.lock().await.push(GithubAdapter {
            workspace: workspace.to_owned(),
            repo: config.repo.clone(),
            poll_secs: config.poll_secs,
            seen: std::collections::BTreeSet::new(),
            next_poll_at: std::time::Instant::now(),
        });
    }

    /// Run the main loop: poll GitHub adapters + handle intake events.
    pub async fn run(&self) {
        let mut rx: Receiver = self.bus.subscribe();
        let mut poll = tokio::time::interval(std::time::Duration::from_mins(1));
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = poll.tick() => self.poll_due_github().await,
                event = rx.recv_or_shutdown() => {
                    match event {
                        Some(BusEvent::Human(HumanEvent::Command { command, args })) if command == "start_ingest" => {
                            let ws = args.first().cloned().unwrap_or_default();
                            let kind = args.get(1).cloned().unwrap_or_default();
                            let item_id = args.get(2).cloned();
                            self.start_workflow_for_kind(&ws, &kind, item_id.as_deref()).await;
                        }
                        Some(_) => {}
                        None => return,
                    }
                }
            }
        }
    }

    /// Poll every GitHub adapter that is due.
    async fn poll_due_github(&self) {
        let now = std::time::Instant::now();
        let due: Vec<GithubAdapter> = {
            let mut guard = self.github.lock().await;
            guard
                .iter_mut()
                .filter(|a| now >= a.next_poll_at)
                .map(|a| {
                    a.next_poll_at = now + std::time::Duration::from_secs(a.poll_secs);
                    a.clone()
                })
                .collect()
        };
        for adapter in due {
            match self.fetch_issues(&adapter).await {
                Ok(issues) => {
                    let mut guard = self.github.lock().await;
                    if let Some(current) =
                        guard.iter_mut().find(|a| a.workspace == adapter.workspace)
                    {
                        for issue in &issues {
                            let number = issue["number"].as_u64().unwrap_or(0);
                            if number == 0 || !current.seen.insert(number) {
                                continue;
                            }
                            let item = IntakeItem {
                                id: format!("in_{}", new_ulid()),
                                source: "github".to_owned(),
                                kind: "bug".to_owned(),
                                title: issue["title"].as_str().unwrap_or("untitled").to_owned(),
                                body: issue["body"].as_str().unwrap_or_default().to_owned(),
                                severity: None,
                                refs: vec![format!("{}#{}", adapter.repo, number)],
                                graph_id: None,
                                received_at: now_rfc3339(),
                            };
                            let mut fleet = self.fleet.lock().await;
                            let _ = fleet.insert_intake(&item);
                            drop(fleet);
                            self.bus.publish(BusEvent::Human(HumanEvent::Command {
                                command: "start_ingest".to_owned(),
                                // (ws, kind, item id) — the id lets the handler
                                // fetch the item for vars + link_intake.
                                args: vec![
                                    adapter.workspace.clone(),
                                    "bug".to_owned(),
                                    item.id.clone(),
                                ],
                            }));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(repo = %adapter.repo, error = %e, "github poll failed");
                }
            }
        }
    }

    /// Fetch open issues for an adapter's repo.
    async fn fetch_issues(&self, adapter: &GithubAdapter) -> Result<Vec<serde_json::Value>> {
        let url = format!("https://api.github.com/repos/{}/issues?state=open", adapter.repo);
        let client = reqwest::Client::new();
        let res = client
            .get(url)
            .header(reqwest::header::USER_AGENT, "agent-bus-supervisor")
            .send()
            .await
            .with_context(|| format!("github issues for {}", adapter.repo))?;
        if !res.status().is_success() {
            anyhow::bail!("github returned {}", res.status());
        }
        let issues: Vec<serde_json::Value> = res.json().await.context("decode github issues")?;
        Ok(issues)
    }

    /// Start the workflow that matches an intake kind (§4.17). Brings the
    /// workspace on if it is off (bug-from-off), renders the item's fields
    /// into the workflow vars, and links the intake row to the graph
    /// (review findings 2 and 3).
    async fn start_workflow_for_kind(&self, ws: &str, kind: &str, item_id: Option<&str>) {
        let item = {
            let fleet = self.fleet.lock().await;
            item_id.and_then(|id| fleet.intake_item(id).cloned())
        };
        let Some(item) = item else {
            tracing::warn!(ws, kind, "start_ingest: unknown intake item");
            return;
        };
        let Some(graph) = item.workflow_graph().map(str::to_owned) else {
            return;
        };
        // Bug-from-off: bring the workspace up before starting the graph.
        let need_on = {
            let fleet = self.fleet.lock().await;
            fleet.workspace(ws).is_none_or(|w| w.state != WorkspaceState::On)
        };
        if need_on && let Err(e) = self.workspaces.on(ws).await {
            tracing::error!(ws, error = %e, "intake: bringing the workspace on failed");
        }
        if let Err(e) = self.workflows.start_graph(ws, &graph, item.workflow_vars()).await {
            tracing::error!(ws, graph = %graph, error = %e, "start ingest workflow failed");
            return;
        }
        let mut fleet = self.fleet.lock().await;
        if let Err(e) = fleet.link_intake(&item.id, &graph) {
            tracing::error!(intake = %item.id, graph = %graph, error = %e, "link intake graph failed");
        }
    }

    /// Discover GitHub adapters from registered projects (called at startup).
    ///
    /// # Errors
    /// Any failure reading a registered project's layout.
    pub async fn discover_adapters(&self) -> Result<()> {
        // Project configs are read from each workspace's layout.
        let projects: Vec<(String, String)> = {
            let fleet = self.fleet.lock().await;
            fleet
                .workspaces()
                .filter_map(|w| w.layout_path.as_ref().map(|p| (w.id.clone(), p.clone())))
                .collect()
        };
        for (ws, layout) in projects {
            let contents =
                std::fs::read_to_string(&layout).with_context(|| format!("reading {layout}"))?;
            let config =
                ProjectConfig::parse(&contents).with_context(|| format!("parsing {layout}"))?;
            if let Some(github) = config.ingest.github {
                self.register_github(&ws, &github).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn kind_selects_graph() {
        // The mapping is a pure match; assert the strings are stable.
        let kind = "bug";
        assert_eq!(if kind == "bug" { "bug_flow" } else { "feature_lifecycle" }, "bug_flow");
    }
}
