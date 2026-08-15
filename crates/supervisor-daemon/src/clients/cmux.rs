//! The cmux client (C5): drives the cmux app via its CLI contract (§7.2).
//!
//! All calls shell out to the `cmux` binary (path from config; `CMUX_SOCKET_PATH`
//! / `CMUX_SOCKET_PASSWORD` respected). Handles are the stable UUID forms
//! (`workspace:<id>`, `surface:<id>`). Verified contract: `new-surface` takes
//! **no `--command`** — create the terminal surface with `--working-directory`,
//! then `send` the attach command.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

/// A stable handle for a cmux workspace.
pub type CmuxHandle = String;

/// A workspace as returned by `cmux list-workspaces`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CmuxWorkspace {
    pub id: String,
    pub name: Option<String>,
}

/// The cmux client contract (§4.4).
#[async_trait]
pub trait CmuxClient: Send + Sync {
    /// `cmux ping` — connectivity.
    async fn ping(&self) -> Result<()>;
    /// `cmux list-workspaces` (or `tree --all`).
    async fn list_workspaces(&self) -> Result<Vec<CmuxWorkspace>>;
    /// `cmux new-workspace --name <n> --cwd <path>`.
    async fn new_workspace(&self, name: &str, cwd: &Path) -> Result<CmuxHandle>;
    /// `cmux new-surface --type terminal --working-directory <path>` (no
    /// `--command`; verified).
    async fn new_surface(&self, ws: &CmuxHandle, working_dir: &Path) -> Result<CmuxHandle>;
    /// `cmux send "<text>"` then Enter — used to run `opencode attach --session`
    /// in a foreground pane.
    async fn send_cmd(&self, ws: &CmuxHandle, surface: &CmuxHandle, text: &str) -> Result<()>;
    /// `cmux focus-pane`.
    async fn focus_pane(&self, ws: &CmuxHandle, pane: &CmuxHandle) -> Result<()>;
    /// `cmux select-workspace`.
    async fn select_workspace(&self, ws: &CmuxHandle) -> Result<()>;
    /// `cmux close-surface`.
    async fn close_surface(&self, ws: &CmuxHandle, surface: &CmuxHandle) -> Result<()>;
    /// `cmux close-workspace`.
    async fn close_workspace(&self, ws: &CmuxHandle) -> Result<()>;
    /// `cmux read-screen --lines N`.
    async fn read_screen(&self, ws: &CmuxHandle, surface: &CmuxHandle) -> Result<String>;
    /// `cmux send` arbitrary text.
    async fn send(&self, ws: &CmuxHandle, surface: &CmuxHandle, text: &str) -> Result<()>;
    /// `cmux send-key`.
    async fn send_key(&self, ws: &CmuxHandle, surface: &CmuxHandle, key: &str) -> Result<()>;
    /// `cmux notify`.
    async fn notify(&self, ws: &CmuxHandle, title: &str, body: &str) -> Result<()>;
}

/// The real client, shelling out to the cmux binary.
pub struct ProcessCmux {
    bin: String,
    /// The running cmux app's workspace, when known.
    workspaces: std::sync::Mutex<std::collections::BTreeMap<String, CmuxWorkspace>>,
}

impl ProcessCmux {
    /// Build a client over the `cmux` binary at `bin`.
    #[must_use]
    pub fn new(bin: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            workspaces: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    async fn run(&self, args: &[&str]) -> Result<std::process::Output> {
        let output = tokio::process::Command::new(&self.bin)
            .args(args)
            .output()
            .await
            .with_context(|| format!("running {} {args:?}", self.bin))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("cmux {args:?} failed: {stderr}");
        }
        Ok(output)
    }

    fn stdout(output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

#[async_trait]
impl CmuxClient for ProcessCmux {
    async fn ping(&self) -> Result<()> {
        let output = self.run(&["ping"]).await?;
        if output.status.success() {
            Ok(())
        } else {
            anyhow::bail!("cmux ping failed: {}", String::from_utf8_lossy(&output.stderr))
        }
    }

    async fn list_workspaces(&self) -> Result<Vec<CmuxWorkspace>> {
        let output = self.run(&["list-workspaces", "--json"]).await?;
        let out = Self::stdout(&output);
        if out.is_empty() {
            return Ok(Vec::new());
        }
        // A3 (caught live): `cmux list-workspaces --json` returns an object
        // `{ "workspaces": [ {id, custom_title, …} ] }`, not a bare array,
        // and the supervisor-created name lives in `custom_title`. The old
        // code decoded a bare array — it always failed, so adoption fell back
        // to creating a duplicate workspace on every `on()`.
        Ok(parse_list_workspaces(&out)?)
    }

    async fn new_workspace(&self, name: &str, cwd: &Path) -> Result<CmuxHandle> {
        let output = self
            .run(&["new-workspace", "--name", name, "--cwd", &cwd.to_string_lossy(), "--json"])
            .await?;
        let handle = extract_handle(&Self::stdout(&output), "workspace")
            .ok_or_else(|| anyhow::anyhow!("cmux new-workspace returned no workspace handle"))?;
        let mut cache = self.workspaces.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(
            name.to_owned(),
            CmuxWorkspace { id: handle.clone(), name: Some(name.to_owned()) },
        );
        Ok(handle)
    }

    async fn new_surface(&self, ws: &CmuxHandle, working_dir: &Path) -> Result<CmuxHandle> {
        let output = self
            .run(&[
                "new-surface",
                "--type",
                "terminal",
                "--workspace",
                ws,
                "--working-directory",
                &working_dir.to_string_lossy(),
                "--json",
            ])
            .await?;
        let handle = extract_handle(&Self::stdout(&output), "surface")
            .ok_or_else(|| anyhow::anyhow!("cmux new-surface returned no surface handle"))?;
        Ok(handle)
    }

    async fn send_cmd(&self, ws: &CmuxHandle, surface: &CmuxHandle, text: &str) -> Result<()> {
        self.send(ws, surface, text).await?;
        self.send_key(ws, surface, "Enter").await
    }

    async fn focus_pane(&self, ws: &CmuxHandle, pane: &CmuxHandle) -> Result<()> {
        self.run(&["focus-pane", "--workspace", ws, "--pane", pane]).await.map(|_| ())
    }

    async fn select_workspace(&self, ws: &CmuxHandle) -> Result<()> {
        self.run(&["select-workspace", "--workspace", ws]).await.map(|_| ())
    }

    async fn close_surface(&self, ws: &CmuxHandle, surface: &CmuxHandle) -> Result<()> {
        self.run(&["close-surface", "--workspace", ws, "--surface", surface]).await.map(|_| ())
    }

    async fn close_workspace(&self, ws: &CmuxHandle) -> Result<()> {
        self.run(&["close-workspace", "--workspace", ws]).await.map(|_| ())
    }

    async fn read_screen(&self, ws: &CmuxHandle, surface: &CmuxHandle) -> Result<String> {
        let output = self
            .run(&["read-screen", "--workspace", ws, "--surface", surface, "--lines", "100"])
            .await?;
        Ok(Self::stdout(&output))
    }

    async fn send(&self, ws: &CmuxHandle, surface: &CmuxHandle, text: &str) -> Result<()> {
        self.run(&["send", "--workspace", ws, "--surface", surface, text]).await.map(|_| ())
    }

    async fn send_key(&self, ws: &CmuxHandle, surface: &CmuxHandle, key: &str) -> Result<()> {
        self.run(&["send-key", "--workspace", ws, "--surface", surface, key]).await.map(|_| ())
    }

    async fn notify(&self, ws: &CmuxHandle, title: &str, body: &str) -> Result<()> {
        self.run(&["notify", "--workspace", ws, "--title", title, "--body", body]).await.map(|_| ())
    }
}

/// Extract a stable handle from cmux JSON or text output.
///
/// cmux returns `{"surface_ref": "surface:45", "pane_ref": "pane:16", ...}`
/// (or plain text `OK surface:46 pane:16 workspace:7`), so both the `*_ref`
/// Parse the real `cmux list-workspaces --json` output (A3): an object with
/// a `workspaces` array whose entries carry `id` + `custom_title` (the name
/// `new_workspace --name X` sets). A bare-array input is also tolerated.
fn parse_list_workspaces(out: &str) -> Result<Vec<CmuxWorkspace>> {
    let value: serde_json::Value =
        serde_json::from_str(out).context("decode cmux list-workspaces")?;
    let arr = value
        .get("workspaces")
        .or_else(|| value.get("windows"))
        .or_else(|| value.as_array().map(|_| &value))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .into_iter()
        .filter_map(|w| {
            let id = w.get("id")?.as_str()?.to_owned();
            let name = w
                .get("custom_title")
                .or_else(|| w.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            Some(CmuxWorkspace { id, name })
        })
        .collect())
}

/// Extract a handle from cmux output: prefer JSON `*_ref`/`id` fields, then
/// the first whitespace-separated token of the requested kind (e.g.
/// `surface:46`). Returns `None` when no handle is present — callers must
/// fail loudly rather than inventing a bogus `{kind}:0` (review minor).
#[must_use]
pub fn extract_handle(output: &str, kind: &str) -> Option<CmuxHandle> {
    // Prefer JSON `*_ref` / `id` fields.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output) {
        for key in ["id", "surface_ref", "pane_ref", "workspace_ref", "window_ref"] {
            if let Some(handle) = value.get(key).and_then(serde_json::Value::as_str) {
                return Some(handle.to_owned());
            }
        }
    }
    // Fallback: the first whitespace-separated token of the requested kind,
    // e.g. `surface:46` for a new-surface call.
    output.split_whitespace().find(|s| s.starts_with(&format!("{kind}:"))).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_extraction_prefers_json_id() {
        assert_eq!(
            extract_handle(r#"{"id":"workspace:7","name":"iot"}"#, "workspace"),
            Some("workspace:7".to_owned())
        );
    }

    #[test]
    fn handle_extraction_reads_surface_ref() {
        let out = r#"{"pane_ref":"pane:16","surface_ref":"surface:45","type":"terminal","window_ref":"window:1","workspace_ref":"workspace:7"}"#;
        assert_eq!(extract_handle(out, "surface"), Some("surface:45".to_owned()));
    }

    #[test]
    fn handle_extraction_falls_back_to_text_token() {
        assert_eq!(
            extract_handle("OK surface:46 pane:16 workspace:7\n", "surface"),
            Some("surface:46".to_owned())
        );
        assert_eq!(
            extract_handle("created workspace:7\n", "workspace"),
            Some("workspace:7".to_owned())
        );
    }

    #[test]
    fn handle_extraction_defaults() {
        assert_eq!(extract_handle("(no output)", "workspace"), None, "no bogus surface:0 fallback");
    }
}

#[cfg(test)]
mod list_workspaces_tests {
    use super::*;

    #[test]
    fn parses_the_real_cmux_object_shape() {
        // A3: the actual `list-workspaces --json` shape — object with a
        // `workspaces` array, name in `custom_title`. The old decoder
        // expected a bare array and always failed, so adoption never ran.
        let out = r#"{
          "window_ref": "window:1",
          "workspaces": [
            { "id": "UUID-1", "custom_title": "smoke-i31", "current_directory": "/x" },
            { "id": "UUID-2", "custom_title": "AI_Tutor", "current_directory": "/y" }
          ]
        }"#;
        let ws = parse_list_workspaces(out).unwrap();
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].id, "UUID-1");
        assert_eq!(ws[0].name.as_deref(), Some("smoke-i31"));
        assert_eq!(ws[1].name.as_deref(), Some("AI_Tutor"));
    }

    #[test]
    fn tolerates_a_bare_array() {
        let ws = parse_list_workspaces(r#"[{"id":"a","name":"x"}]"#).unwrap();
        assert_eq!(ws[0].id, "a");
        assert_eq!(ws[0].name.as_deref(), Some("x"));
    }
}
