//! The manager client (C11): the background LLM decision engine.
//!
//! The manager is a **background** opencode session on the supervisor server
//! (port 4199), driven programmatically with structured output and resolved
//! with the same layered fallback as the ACK contract (§4.9): structured →
//! parse-final-JSON → regex → re-ask once with a stricter instruction → surface
//! to the dashboard. It has no pane, no TUI, and no slash commands (that is the
//! supervisor agent, C13).

use std::sync::Mutex;

use anyhow::{Context, Result};
use supervisor_core::Situation;
use supervisor_core::types::{AgentId, SessionId};

use crate::clients::driver::OutputFormat;
use crate::clients::opencode::{OpencodeClient, last_structured};

/// The supervisor workspace port: the manager session lives here.
pub const SUPERVISOR_WORKSPACE_PORT: u16 = 4199;

/// The decision schema the manager must produce (§4.12).
const DECISION_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "action": { "enum": ["done", "rerun", "skip", "split", "post"] },
    "to": { "type": "string" },
    "body": { "type": "string" },
    "reason": { "type": "string" },
    "confidence": { "type": "number" }
  },
  "required": ["action", "reason", "confidence"]
}"#;

/// The manager's structured decision.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagerDecision {
    /// `done | rerun | skip | split | post`.
    pub action: String,
    pub to: Option<AgentId>,
    pub body: Option<String>,
    pub reason: String,
    /// The manager's own confidence, 0–1. Below 0.5 the escalation is treated
    /// as unresolved and surfaced to the human (§4.12).
    pub confidence: f64,
}

impl ManagerDecision {
    /// Parse a decision from an arbitrary JSON value.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let action = value.get("action")?.as_str()?.to_owned();
        if !matches!(action.as_str(), "done" | "rerun" | "skip" | "split" | "post") {
            return None;
        }
        let reason =
            value.get("reason").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        let confidence = value.get("confidence").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
        Some(Self {
            action,
            to: value.get("to").and_then(serde_json::Value::as_str).map(str::to_owned),
            body: value.get("body").and_then(serde_json::Value::as_str).map(str::to_owned),
            reason,
            confidence,
        })
    }

    /// Parse a decision from a manager message list, scanning assistant
    /// structured fields then text.
    #[must_use]
    pub fn from_messages(messages: &[crate::clients::opencode::Message]) -> Option<Self> {
        // Structured fields first.
        if let Some(structured) = last_structured(messages)
            && let Some(decision) = Self::from_json(&structured)
        {
            return Some(decision);
        }
        // Then parse the final text as JSON.
        for message in messages.iter().rev() {
            for part in &message.parts {
                if let Some(text) = &part.text
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
                    && let Some(decision) = Self::from_json(&value)
                {
                    return Some(decision);
                }
            }
        }
        // Regex fallback (§4.12 layered resolution, review I-9): a decision
        // JSON object embedded in prose (e.g. fenced/markdown) still parses.
        for message in messages.iter().rev() {
            for part in &message.parts {
                let Some(text) = &part.text else { continue };
                for span in json_object_spans(text) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(span)
                        && let Some(decision) = Self::from_json(&value)
                    {
                        return Some(decision);
                    }
                }
            }
        }
        None
    }
}

/// Spans of balanced `{ ... }` in text (mirrors the ACK resolver's extraction).
fn json_object_spans(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0
                        && let Some(s) = start
                    {
                        spans.push(&text[s..=i]);
                    }
                }
            }
            _ => {}
        }
    }
    spans
}

/// The manager client: ensures a manager session on the supervisor server and
/// drives escalations.
pub struct ManagerClient {
    /// The opencode client for the supervisor workspace server.
    client: OpencodeClient,
    /// The manager session id, created lazily.
    session_id: Mutex<Option<SessionId>>,
}

impl ManagerClient {
    /// Build a manager client over the supervisor server.
    #[must_use]
    pub fn new(client: OpencodeClient) -> Self {
        Self { client, session_id: Mutex::new(None) }
    }

    /// Build a manager client for `127.0.0.1:<port>`.
    ///
    /// # Errors
    /// Invalid base URL.
    pub fn connect(port: u16, password: &str) -> Result<Self> {
        let client = OpencodeClient::new(port, password)?;
        Ok(Self::new(client))
    }

    /// Ensure the manager session exists; create it lazily.
    async fn ensure_session(&self) -> Result<SessionId> {
        let session_id =
            self.session_id.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        if let Some(session) = session_id
            && self.client.get_session(&session).await.ok().flatten().is_some()
        {
            return Ok(session);
        }
        let session = self.client.create_session("supervisor/manager", Some("manager")).await?;
        *self.session_id.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(session.id.clone());
        Ok(session.id)
    }

    /// Escalate a compact escalation record and resolve the manager's decision
    /// with the layered fallback (§4.12).
    ///
    /// # Errors
    /// Session/transport failures.
    pub async fn escalate(
        &self,
        situation: &Situation,
        candidates: Vec<String>,
    ) -> Result<Option<ManagerDecision>> {
        let session = self.ensure_session().await?;
        let prompt = build_escalation_prompt(situation, &candidates);
        let format = OutputFormat::json_schema(
            serde_json::from_str(DECISION_SCHEMA).context("parse decision schema")?,
        );
        // Try structured output first; some models reject it, so send plain if
        // the prompt still works. The layered fallback handles the rest.
        if self
            .client
            .prompt_async(&session, &prompt, Some("manager"), Some(&format))
            .await
            .is_err()
        {
            self.client.prompt_async(&session, &prompt, Some("manager"), None).await?;
        }
        // Wait for the turn to finish, reading the decision each round. The
        // session-status map omits idle sessions, so the manager is idle once
        // its id disappears — stop early then. A generous 60s cap covers a
        // slow model (review I-9: the old 6s budget discarded correct
        // decisions arriving at t=8s).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut saw_idle = false;
        loop {
            let messages = self.client.messages(&session, 10).await?;
            if let Some(decision) = ManagerDecision::from_messages(&messages) {
                return Ok(Some(decision));
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let idle = self
                .client
                .session_status()
                .await
                .map(|status| !status.contains_key(&session))
                .unwrap_or(false);
            if idle {
                saw_idle = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
        if !saw_idle && tokio::time::Instant::now() >= deadline {
            // The manager never went idle; give up rather than queue an
            // unbounded follow-up. The escalation surfaces to the dashboard.
            return Ok(None);
        }
        // One re-ask with a stricter instruction (I-9), then a final read.
        // The manager is idle here (its previous turn ended), so this prompt
        // gets a fresh turn.
        let reask = format!(
            "{prompt}\nIf your previous reply was not a valid JSON object, reply now with ONLY the JSON object and nothing else."
        );
        self.client.prompt_async(&session, &reask, Some("manager"), None).await?;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let messages = self.client.messages(&session, 10).await?;
            if let Some(decision) = ManagerDecision::from_messages(&messages) {
                return Ok(Some(decision));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
    }
}

/// The compact escalation record sent to the manager (§4.12).
fn build_escalation_prompt(situation: &Situation, candidates: &[String]) -> String {
    format!(
        "You are the supervisor's decision engine. Decide how to handle this escalation.\n\
         Situation: agent={} ws={} state={:?} signals=[{}] inbox_depth={}\n\
         Candidates: {}\n\
         Reply with ONLY a JSON object: {{\"action\":\"done|rerun|skip|split|post\",\"to\":\"agent-id?\",\"body\":\"instruction if post\",\"reason\":\"short justification\",\"confidence\":0.0}}\n\
         confidence 0.0–1.0 is YOUR confidence in the decision; below 0.5 the supervisor treats it as unresolved.",
        situation.agent,
        situation.ws,
        situation.state,
        situation.signals.iter().map(supervisor_core::Signal::name).collect::<Vec<_>>().join(","),
        situation.inbox_depth,
        candidates.join(", "),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;
    use crate::clients::opencode::{Message, Part};

    fn message(text: &str) -> Message {
        Message {
            info: serde_json::json!({}),
            parts: vec![Part {
                kind: "text".to_owned(),
                text: Some(text.to_owned()),
                structured: None,
            }],
        }
    }

    #[test]
    fn structured_decision_parses() {
        let decision = ManagerDecision::from_json(&serde_json::json!({
            "action": "rerun", "reason": "transient", "confidence": 0.9
        }))
        .unwrap();
        assert_eq!(decision.action, "rerun");
        assert_eq!(decision.confidence, 0.9);
        assert_eq!(decision.to, None);
    }

    #[test]
    fn post_decision_parses_target_and_body() {
        let decision = ManagerDecision::from_json(&serde_json::json!({
            "action": "post", "to": "tester_01", "body": "retry once",
            "reason": "flaky", "confidence": 0.7
        }))
        .unwrap();
        assert_eq!(decision.to.as_deref(), Some("tester_01"));
        assert_eq!(decision.body.as_deref(), Some("retry once"));
    }

    #[test]
    fn invalid_action_is_rejected() {
        assert!(
            ManagerDecision::from_json(&serde_json::json!({
                "action": "explode", "reason": "x", "confidence": 0.5
            }))
            .is_none()
        );
    }

    #[test]
    fn decision_parses_from_message_text() {
        let messages = vec![
            message("Let me think..."),
            message(r#"{"action":"skip","reason":"cosmetic","confidence":0.8}"#),
        ];
        let decision = ManagerDecision::from_messages(&messages).unwrap();
        assert_eq!(decision.action, "skip");
    }

    #[test]
    fn escalation_prompt_names_candidates() {
        let situation = Situation {
            ws: "iot".to_owned(),
            agent: "dev_01".to_owned(),
            agent_role: "dev".to_owned(),
            state: supervisor_core::types::AgentState::Error,
            reason: Some("step.failed".to_owned()),
            signals: Vec::new(),
            node: None,
            inbox_depth: 0,
            last_output: None,
            state_confidence: 1.0,
        };
        let prompt = build_escalation_prompt(&situation, &["rerun".to_owned(), "skip".to_owned()]);
        assert!(prompt.contains("iot"));
        assert!(prompt.contains("rerun"));
        assert!(!prompt.contains("json_schema"));
    }
}
