//! The opencode HTTP client (C6) and the opencode driver (§4.5, §4.7).
//!
//! Base URL per workspace: `http://127.0.0.1:<port>`, authenticated with basic
//! auth (`user: opencode`, password = `OPENCODE_SERVER_PASSWORD`, which the
//! supervisor passes to each `serve` process via env).
//!
//! Verified contract (§7.1): `prompt_async` returns 204 and queues serially
//! per session; `GET /session/status` returns a map that **omits idle
//! sessions** (idle arrives only on SSE); `format: json_schema` is accepted
//! but model-dependent — never rely on it alone.

use std::collections::HashMap;

use anyhow::{Context, Result};
use reqwest::Url;
use supervisor_core::types::{AgentState, SessionId, SessionStatus};

use super::driver::{AgentDriver, AgentRef, DriverKind, OutputFormat, SendReceipt};
use async_trait::async_trait;

/// A message part as returned by `GET /session/{id}/message`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
    pub structured: Option<serde_json::Value>,
}

/// A message row `{info, parts}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub info: serde_json::Value,
    pub parts: Vec<Part>,
}

/// A session as returned by `POST /session` / `GET /session/{id}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: SessionId,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
}

/// A status row inside the `/session/status` map.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusRow {
    #[serde(rename = "type")]
    pub status: SessionStatus,
}

impl SessionStatusRow {
    /// Parse a status string into the enum.
    #[must_use]
    pub fn parse(status: &str) -> Self {
        let status = match status {
            "busy" => SessionStatus::Busy,
            "retry" => SessionStatus::Retry,
            _ => SessionStatus::Idle,
        };
        Self { status }
    }
}

/// The opencode HTTP client for one workspace server.
#[derive(Debug, Clone)]
pub struct OpencodeClient {
    base: Url,
    client: reqwest::Client,
}

impl OpencodeClient {
    /// Build a client for `http://127.0.0.1:<port>` with the given password.
    ///
    /// # Errors
    /// Invalid base URL or header construction failure.
    pub fn new(port: u16, password: &str) -> Result<Self> {
        let base = Url::parse(&format!("http://127.0.0.1:{port}"))
            .with_context(|| format!("invalid opencode base URL for port {port}"))?;
        let mut headers = reqwest::header::HeaderMap::new();
        let credentials = base64_credentials(password);
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Basic {credentials}"))
                .context("build basic auth header")?,
        );
        // Explicit timeouts (review C-3): a hung `opencode serve` must fail a
        // call, not block the caller forever (which previously let SSE frames
        // pile up and Lagged every subscriber). The SSE /event stream gets its
        // own long read timeout below.
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building reqwest client")?;
        Ok(Self { base, client })
    }

    /// A client over a caller-supplied reqwest client (for tests / proxies).
    #[must_use]
    pub fn with_client(base: Url, client: reqwest::Client) -> Self {
        Self { base, client }
    }

    /// GET /global/health. `Ok(false)` when the server answers non-200.
    ///
    /// # Errors
    /// Transport failures.
    pub async fn health(&self) -> Result<bool> {
        let url = self.base.join("/global/health").context("join health url")?;
        let res = self.client.get(url).send().await.context("GET /global/health")?;
        Ok(res.status().is_success())
    }

    /// POST /session.
    ///
    /// # Errors
    /// Transport failures or a non-2xx response.
    pub async fn create_session(&self, title: &str, agent: Option<&str>) -> Result<Session> {
        let url = self.base.join("/session").context("join session url")?;
        let body = serde_json::json!({
            "title": title,
            "agent": agent,
        });
        let res = self.client.post(url).json(&body).send().await.context("POST /session")?;
        let session = parse_expected(res, "create_session").await?;
        let session: Session = serde_json::from_value(session).context("decode session")?;
        Ok(session)
    }

    /// GET /session/{id}. `Ok(None)` when the session no longer exists (404).
    ///
    /// # Errors
    /// Transport failures.
    pub async fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let url = self.base.join(&format!("/session/{id}")).context("join session url")?;
        let res = self.client.get(url).send().await.context("GET /session/{id}")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = parse_expected(res, "get_session").await?;
        let session: Session = serde_json::from_value(body).context("decode session")?;
        Ok(Some(session))
    }

    /// GET /session/status. Idle sessions are omitted by the server; a known
    /// session absent from the map is idle.
    ///
    /// # Errors
    /// Transport failures.
    pub async fn session_status(&self) -> Result<HashMap<SessionId, SessionStatusRow>> {
        let url = self.base.join("/session/status").context("join status url")?;
        let res = self.client.get(url).send().await.context("GET /session/status")?;
        let body = parse_expected(res, "session_status").await?;
        let map: HashMap<String, SessionStatusRow> =
            serde_json::from_value(body).context("decode session status")?;
        Ok(map)
    }

    /// `POST /session/{id}/prompt_async`. Returns once the server has accepted
    /// (204); prompts queue serially per session.
    ///
    /// # Errors
    /// Transport failures or a non-2xx response.
    pub async fn prompt_async(
        &self,
        id: &str,
        text: &str,
        agent: Option<&str>,
        format: Option<&OutputFormat>,
    ) -> Result<()> {
        let url =
            self.base.join(&format!("/session/{id}/prompt_async")).context("join prompt url")?;
        let mut body = serde_json::json!({
            "parts": [{ "type": "text", "text": text }],
            "agent": agent,
        });
        if let Some(fmt) = format {
            body["format"] = serde_json::to_value(fmt).context("encode output format")?;
        }
        let res = self.client.post(url).json(&body).send().await.context("POST prompt_async")?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            anyhow::bail!("prompt_async returned {status}: {text}");
        }
        Ok(())
    }

    /// GET /session/{id}/message?limit=.
    ///
    /// # Errors
    /// Transport failures.
    pub async fn messages(&self, id: &str, limit: usize) -> Result<Vec<Message>> {
        let url = self
            .base
            .join(&format!("/session/{id}/message?limit={limit}"))
            .context("join messages url")?;
        let res = self.client.get(url).send().await.context("GET messages")?;
        let body = parse_expected(res, "messages").await?;
        serde_json::from_value(body).context("decode messages")
    }

    /// POST /session/{id}/permissions/{pid}.
    ///
    /// # Errors
    /// Transport failures or a non-2xx response.
    pub async fn respond_permission(
        &self,
        id: &str,
        permission_id: &str,
        allow: bool,
    ) -> Result<()> {
        let url = self
            .base
            .join(&format!("/session/{id}/permissions/{permission_id}"))
            .context("join permission url")?;
        let res = self
            .client
            .post(url)
            .json(&serde_json::json!({ "response": if allow { "allow" } else { "deny" } }))
            .send()
            .await
            .context("POST permission")?;
        ensure_success(res, "respond_permission").await
    }

    /// POST /session/{id}/abort.
    ///
    /// # Errors
    /// Transport failures or a non-2xx response.
    pub async fn abort(&self, id: &str) -> Result<()> {
        let url = self.base.join(&format!("/session/{id}/abort")).context("join abort url")?;
        let res = self.client.post(url).send().await.context("POST abort")?;
        ensure_success(res, "abort").await
    }

    /// POST /session/{id}/revert.
    ///
    /// # Errors
    /// Transport failures or a non-2xx response.
    pub async fn revert(&self, id: &str) -> Result<()> {
        let url = self.base.join(&format!("/session/{id}/revert")).context("join revert url")?;
        let res = self.client.post(url).send().await.context("POST revert")?;
        ensure_success(res, "revert").await
    }

    /// POST /session/{id}/summarize.
    ///
    /// # Errors
    /// Transport failures or a non-2xx response.
    pub async fn summarize(&self, id: &str) -> Result<()> {
        let url =
            self.base.join(&format!("/session/{id}/summarize")).context("join summarize url")?;
        let res = self
            .client
            .post(url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("POST summarize")?;
        ensure_success(res, "summarize").await
    }

    /// The base URL, for building `/event` SSE streams.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The authenticated HTTP client (for the SSE stream, which needs the same
    /// basic auth).
    #[must_use]
    pub fn http_client(&self) -> &reqwest::Client {
        &self.client
    }
}

async fn ensure_success(res: reqwest::Response, op: &str) -> Result<()> {
    if res.status().is_success() {
        return Ok(());
    }
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    anyhow::bail!("{op} returned {status}: {text}")
}

async fn parse_expected(res: reqwest::Response, op: &str) -> Result<serde_json::Value> {
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        anyhow::bail!("{op} returned {status}: {text}");
    }
    res.json().await.with_context(|| format!("decode {op} response"))
}

/// Build the basic-auth header value for `opencode:<password>`.
fn base64_credentials(password: &str) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    STANDARD.encode(format!("opencode:{password}"))
}

/// The opencode driver (§4.7): `send` → `prompt_async`, `read_structured` →
/// the last assistant message's structured field, `status` → the status map +
/// SSE, `abort` → `/abort`.
#[derive(Clone)]
pub struct OpencodeDriver {
    client: OpencodeClient,
}

impl OpencodeDriver {
    /// Build a driver over an opencode client.
    #[must_use]
    pub fn new(client: OpencodeClient) -> Self {
        Self { client }
    }

    #[must_use]
    pub fn client(&self) -> &OpencodeClient {
        &self.client
    }
}

#[async_trait]
impl AgentDriver for OpencodeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Opencode
    }

    async fn send(
        &self,
        a: &AgentRef,
        msg: &str,
        format: Option<&OutputFormat>,
    ) -> Result<SendReceipt> {
        self.client
            .prompt_async(&a.session_id, msg, None, format)
            .await
            .with_context(|| format!("send to {}/{}", a.ws, a.agent))?;
        Ok(SendReceipt { session_id: a.session_id.clone(), structured_requested: format.is_some() })
    }

    async fn read_last_output(&self, a: &AgentRef, limit: usize) -> Result<String> {
        let messages = self
            .client
            .messages(&a.session_id, limit)
            .await
            .with_context(|| format!("read output from {}/{}", a.ws, a.agent))?;
        Ok(render_last_output(&messages))
    }

    async fn read_structured(&self, a: &AgentRef) -> Result<Option<serde_json::Value>> {
        let messages = self
            .client
            .messages(&a.session_id, 20)
            .await
            .with_context(|| format!("read structured from {}/{}", a.ws, a.agent))?;
        Ok(last_structured(&messages))
    }

    async fn status(&self, a: &AgentRef) -> Result<supervisor_core::types::AgentState> {
        let map = self.client.session_status().await.context("session status")?;
        Ok(map.get(&a.session_id).map_or(AgentState::Idle, |row| match row.status {
            SessionStatus::Busy | SessionStatus::Retry => AgentState::Working,
            SessionStatus::Idle => AgentState::Idle,
        }))
    }

    async fn abort(&self, a: &AgentRef) -> Result<()> {
        self.client
            .abort(&a.session_id)
            .await
            .with_context(|| format!("abort {}/{}", a.ws, a.agent))
    }

    async fn read_transcript(
        &self,
        a: &AgentRef,
        limit: usize,
    ) -> Result<Vec<super::driver::TranscriptMessage>> {
        let messages = self
            .client
            .messages(&a.session_id, limit)
            .await
            .with_context(|| format!("read transcript for {}/{}", a.ws, a.agent))?;
        Ok(messages.iter().map(message_to_transcript).collect())
    }

    async fn respond_permission(
        &self,
        a: &AgentRef,
        permission_id: &str,
        allow: bool,
    ) -> Result<()> {
        self.client
            .respond_permission(&a.session_id, permission_id, allow)
            .await
            .with_context(|| format!("permission response for {}/{}", a.ws, a.agent))
    }
}

/// Render the **last** assistant message's text (the "final output" the ACK
/// contract targets). The whole transcript is not used: a `start_template` may
/// itself contain the ACK JSON shape, and scanning earlier messages would let
/// a prompt false-positive as the completion.
#[must_use]
pub fn render_last_output(messages: &[Message]) -> String {
    let Some(last) = messages.last() else { return String::new() };
    let mut out = String::new();
    for part in &last.parts {
        if let Some(text) = &part.text {
            out.push_str(text);
            out.push('\n');
        }
    }
    out.trim_end().to_owned()
}

/// The last assistant message's structured field, if any.
#[must_use]
pub fn last_structured(messages: &[Message]) -> Option<serde_json::Value> {
    messages.iter().rev().find_map(|m| m.parts.iter().rev().find_map(|p| p.structured.clone()))
}

/// Convert an opencode message row into a transcript row (role + ts + text +
/// usage). Tolerant of the info shape: role/time/usage are read from wherever
/// opencode puts them; missing fields degrade gracefully.
#[must_use]
pub fn message_to_transcript(message: &Message) -> super::driver::TranscriptMessage {
    let role =
        message.info.get("role").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let ts = message
        .info
        .get("time")
        .or_else(|| message.info.get("timestamp"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut text = String::new();
    for part in &message.parts {
        if let Some(t) = &part.text {
            text.push_str(t);
            text.push('\n');
        }
    }
    let usage = message
        .info
        .get("usage")
        .or_else(|| message.info.get("tokens"))
        .and_then(|u| serde_json::from_value::<UsageRow>(u.clone()).ok())
        .map(|u| super::driver::Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
        });
    super::driver::TranscriptMessage { role, ts, text: text.trim_end().to_owned(), usage }
}

/// The usage sub-shape inside an opencode message `info` (camelCase tolerant).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageRow {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(parts: Vec<Part>) -> Message {
        Message { info: serde_json::json!({}), parts }
    }

    #[test]
    fn render_last_output_uses_only_the_final_message() {
        let messages = vec![
            message(vec![Part {
                kind: "text".to_owned(),
                text: Some("earlier (the prompt)".to_owned()),
                structured: None,
            }]),
            message(vec![Part {
                kind: "text".to_owned(),
                text: Some("{\"task_id\":\"dev\",\"status\":\"done\"}".to_owned()),
                structured: None,
            }]),
        ];
        assert_eq!(
            render_last_output(&messages),
            "{\"task_id\":\"dev\",\"status\":\"done\"}",
            "a prompt containing the ACK shape must not false-positive"
        );
    }

    #[test]
    fn last_structured_returns_the_most_recent() {
        let messages = vec![
            message(vec![Part {
                kind: "structured".to_owned(),
                text: None,
                structured: Some(serde_json::json!({"task_id": "old"})),
            }]),
            message(vec![Part {
                kind: "structured".to_owned(),
                text: None,
                structured: Some(serde_json::json!({"task_id": "new"})),
            }]),
        ];
        assert_eq!(last_structured(&messages).unwrap()["task_id"], "new");
    }

    #[test]
    fn last_structured_none_when_absent() {
        assert_eq!(last_structured(&[]), None);
    }

    #[test]
    fn status_row_parses() {
        assert_eq!(SessionStatusRow::parse("busy").status, SessionStatus::Busy);
        assert_eq!(SessionStatusRow::parse("retry").status, SessionStatus::Retry);
        assert_eq!(SessionStatusRow::parse("idle").status, SessionStatus::Idle);
        assert_eq!(SessionStatusRow::parse("weird").status, SessionStatus::Idle);
    }

    #[test]
    fn part_and_message_roundtrip() {
        let p = Part { kind: "text".to_owned(), text: Some("hi".to_owned()), structured: None };
        let m = message(vec![p]);
        let back: Message = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.parts[0].text.as_deref(), Some("hi"));
    }
}
