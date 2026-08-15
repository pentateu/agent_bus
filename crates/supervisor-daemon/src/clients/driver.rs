//! The driver abstraction (§4.7): every agent is driven through an
//! [`AgentDriver`] so the supervisor is not married to the opencode API.
//!
//! Today only the opencode driver is implemented; the cmux driver (for
//! harnesses with no API) arrives later and drives panes via `cmux send` /
//! `read-screen`. The workflow engine sees both through the same trait.

use async_trait::async_trait;

use supervisor_core::types::{AgentId, AgentState, SessionId};

/// How a driver reaches an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    Opencode,
    Cmux,
}

/// A scoped reference to an agent for a driver call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRef {
    pub ws: String,
    pub agent: AgentId,
    pub session_id: SessionId,
}

/// A structured-output format request (model-dependent; never the only path).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputFormat {
    pub r#type: String,
    pub schema: serde_json::Value,
}

impl OutputFormat {
    /// A JSON-schema `format` for `prompt_async`.
    #[must_use]
    pub fn json_schema(schema: serde_json::Value) -> Self {
        Self { r#type: "json_schema".to_owned(), schema }
    }
}

/// A receipt from `send` (the transport accepted the turn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReceipt {
    pub session_id: SessionId,
    /// True when the driver could request structured output back.
    pub structured_requested: bool,
}

/// Token usage for one message (None for the cmux driver).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// One transcript row (web UI agent dialog; the usage collector's token
/// source).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptMessage {
    pub role: String,
    pub ts: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// The driver every agent is driven through (§4.7).
#[async_trait]
pub trait AgentDriver: Send + Sync {
    /// Which harness this driver talks to.
    fn kind(&self) -> DriverKind;

    /// Deliver a prompt to the agent's session. The session may be busy; the
    /// server queues serially (opencode) or the pane types into the terminal.
    async fn send(
        &self,
        a: &AgentRef,
        msg: &str,
        format: Option<&OutputFormat>,
    ) -> anyhow::Result<SendReceipt>;

    /// Read the last `limit` lines of output.
    async fn read_last_output(&self, a: &AgentRef, limit: usize) -> anyhow::Result<String>;

    /// Read the last assistant message's structured field, if the driver can.
    async fn read_structured(&self, a: &AgentRef) -> anyhow::Result<Option<serde_json::Value>>;

    /// The agent's current state.
    async fn status(&self, a: &AgentRef) -> anyhow::Result<AgentState>;

    /// Abort the current turn.
    async fn abort(&self, a: &AgentRef) -> anyhow::Result<()>;

    /// The agent's message transcript (web UI dialog + the usage collector's
    /// token source). Default: no transcript (drivers must opt in).
    async fn read_transcript(
        &self,
        _a: &AgentRef,
        _limit: usize,
    ) -> anyhow::Result<Vec<TranscriptMessage>> {
        Ok(Vec::new())
    }

    /// Respond to a tool-permission prompt. Default: unsupported.
    async fn respond_permission(
        &self,
        _a: &AgentRef,
        _permission_id: &str,
        _allow: bool,
    ) -> anyhow::Result<()> {
        anyhow::bail!("this driver does not support automated permission responses")
    }
}
