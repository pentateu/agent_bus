//! The completion / ACK contract resolver (§4.9).
//!
//! Every workflow node's `start_template` instructs the agent that its final
//! message must be a single JSON object:
//!
//! ```text
//! {"task_id":"<task_id>","status":"done|failed|blocked","summary":"<one line>"}
//! ```
//!
//! Human-gate nodes add two optional fields when status is `done`:
//! `approved` and `needs_revision` (`none|small|big`).
//!
//! Completion is resolved in strict order because structured output is
//! model-dependent (thinking-mode models reject `format: json_schema`):
//! **structured → parse final text as JSON → regex ACK line**. A node whose
//! turn ends with no resolvable ack stays `running` until its timeout moves it
//! to `needs_decision`.

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::error::{CoreError, CoreResult};
use crate::types::{AckStatus, Revision};

/// The ACK-line regex, a compile-time constant.
#[allow(clippy::expect_used)]
static ACK_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?m)^\s*ACK\s+(\S+)\s+(done|failed|blocked)(?:\s+(.*))?\s*$")
        .expect("ACK regex is a compile-time constant")
});

/// A resolved completion from the ACK contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub task_id: String,
    pub status: AckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Present only for human-gate nodes when status is `done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
    /// Present only for human-gate nodes when status is `done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_revision: Option<Revision>,
}

/// The layered resolver. `structured` is the last assistant message's
/// structured field (only present when we sent `format` and the driver can
/// read it); `text` is the final output text.
#[must_use]
pub fn resolve_ack(structured: Option<&str>, text: &str) -> Option<Ack> {
    if let Some(json) = structured.and_then(Ack::from_json) {
        return Some(json);
    }
    if let Some(json) = Ack::from_text_json(text) {
        return Some(json);
    }
    Ack::from_regex(text)
}

impl Ack {
    /// Parse a `structured-output` / `JSON-text` ack. The object must carry a
    /// `task_id` and a valid `status`; a valid `approved` must be a bool.
    ///
    /// # Errors
    /// [`CoreError::InvalidAck`] when the JSON is not a well-formed ack.
    pub fn parse(json: &serde_json::Value) -> CoreResult<Self> {
        let task_id = json
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::InvalidAck("missing or empty task_id".to_owned()))?
            .to_owned();
        let status = match json.get("status").and_then(serde_json::Value::as_str) {
            Some("done") => AckStatus::Done,
            Some("failed") => AckStatus::Failed,
            Some("blocked") => AckStatus::Blocked,
            _ => {
                return Err(CoreError::InvalidAck("status must be done|failed|blocked".to_owned()));
            }
        };
        let summary = json.get("summary").and_then(serde_json::Value::as_str).map(str::to_owned);
        let approved = json.get("approved").and_then(serde_json::Value::as_bool);
        let needs_revision = match json.get("needs_revision").and_then(serde_json::Value::as_str) {
            None | Some("none") => None,
            Some("small") => Some(Revision::Small),
            Some("medium") => Some(Revision::Medium),
            Some("big") => Some(Revision::Big),
            _ => {
                return Err(CoreError::InvalidAck(
                    "needs_revision must be none|small|medium|big".to_owned(),
                ));
            }
        };
        Ok(Self { task_id, status, summary, approved, needs_revision })
    }

    /// Parse an already-extracted JSON value.
    #[must_use]
    pub fn from_json(json: &str) -> Option<Self> {
        let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
        Self::parse(&value).ok()
    }

    /// Parse an ack from free text: the whole text as a single JSON object, or
    /// the last `{...}` block found in it.
    #[must_use]
    pub fn from_text_json(text: &str) -> Option<Self> {
        let candidates = json_object_spans(text);
        for span in candidates.into_iter().rev() {
            if let Some(ack) = Self::from_json(span) {
                return Some(ack);
            }
        }
        None
    }

    /// The regex ACK line (universal fallback, incl. the cmux driver):
    ///
    /// `ACK <task_id> done|failed|blocked [summary]` and, for human-gate nodes,
    /// `ACK <task_id> done approved=true|false revision=none|small|big`.
    #[must_use]
    pub fn from_regex(text: &str) -> Option<Self> {
        for cap in ACK_RE.captures_iter(text) {
            let task_id = cap.get(1)?.as_str().to_owned();
            let status = match cap.get(2)?.as_str() {
                "done" => AckStatus::Done,
                "failed" => AckStatus::Failed,
                "blocked" => AckStatus::Blocked,
                _ => continue,
            };
            let rest = cap.get(3).map_or("", |m| m.as_str()).trim();
            let (summary, approved, needs_revision) = parse_ack_tail(rest);
            return Some(Self { task_id, status, summary, approved, needs_revision });
        }
        None
    }
}

/// Parse the trailing `key=value` tokens of a regex ACK line. The summary is
/// the text before the first `key=` token; unknown tokens are ignored.
fn parse_ack_tail(rest: &str) -> (Option<String>, Option<bool>, Option<Revision>) {
    let mut summary = None;
    let mut approved = None;
    let mut revision = None;
    let mut buffer = String::new();
    let mut fields = Vec::new();
    for token in rest.split_whitespace() {
        if let Some((key, value)) = token.split_once('=') {
            fields.push((key.to_owned(), value.to_owned()));
            if !buffer.trim().is_empty() {
                summary = Some(buffer.trim().to_owned());
                buffer.clear();
            }
        } else {
            buffer.push_str(token);
            buffer.push(' ');
        }
    }
    if summary.is_none() && !buffer.trim().is_empty() {
        summary = Some(buffer.trim().to_owned());
    }
    for (key, value) in fields {
        match key.as_str() {
            "approved" => approved = Some(value == "true"),
            "revision" | "needs_revision" => {
                revision = match value.as_str() {
                    "small" => Some(Revision::Small),
                    "medium" => Some(Revision::Medium),
                    "big" => Some(Revision::Big),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    (summary, approved, revision)
}

/// The spans of text that look like `{ ... }` JSON objects (balanced braces),
/// for `JSON-text` parsing.
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
                if let Some(s) = start
                    && depth > 0
                {
                    depth -= 1;
                    if depth == 0 {
                        spans.push(&text[s..=i]);
                        start = None;
                    }
                }
            }
            _ => {}
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_json_parses() {
        let ack =
            resolve_ack(Some(r#"{"task_id":"dev","status":"done","summary":"implemented"}"#), "")
                .expect("structured ack resolves");
        assert_eq!(ack.task_id, "dev");
        assert_eq!(ack.status, AckStatus::Done);
        assert_eq!(ack.summary.as_deref(), Some("implemented"));
    }

    #[test]
    fn thinking_mode_text_json_parses() {
        let text = "I finished.\n{\"task_id\":\"dev\",\"status\":\"done\",\"summary\":\"done\"}\n";
        let ack = resolve_ack(None, text).expect("text JSON resolves");
        assert_eq!(ack.task_id, "dev");
        assert_eq!(ack.status, AckStatus::Done);
    }

    #[test]
    fn regex_ack_line_is_the_fallback() {
        let text = "Done. Wrapping up.\nACK dev_01.fix done fixed the bug\n";
        let ack = resolve_ack(None, text).expect("regex ACK resolves");
        assert_eq!(ack.task_id, "dev_01.fix");
        assert_eq!(ack.status, AckStatus::Done);
        assert_eq!(ack.summary.as_deref(), Some("fixed the bug"));
    }

    #[test]
    fn regex_human_gate_tokens_parse() {
        let text = "ACK hl_gate done approved=false revision=big\n";
        let ack = Ack::from_regex(text).expect("gate ACK parses");
        assert_eq!(ack.approved, Some(false));
        assert_eq!(ack.needs_revision, Some(Revision::Big));
    }

    #[test]
    fn regex_with_revision_none_means_approved_no_loop() {
        let ack = Ack::from_regex("ACK gate done approved=true revision=none\n").unwrap();
        assert_eq!(ack.approved, Some(true));
        assert_eq!(ack.needs_revision, None);
    }

    #[test]
    fn layered_precedence_prefers_structured_over_text() {
        let text = "ACK dev done\n{\"task_id\":\"other\",\"status\":\"failed\"}\n";
        let ack =
            resolve_ack(Some(r#"{"task_id":"structured","status":"blocked"}"#), text).unwrap();
        assert_eq!(ack.task_id, "structured");
        assert_eq!(ack.status, AckStatus::Blocked);
    }

    #[test]
    fn structured_that_parses_as_json_but_bad_ack_falls_through() {
        let text = "{\"not_an_ack\": true}\nACK dev done\n";
        let ack = resolve_ack(Some(r#"{"foo":1}"#), text).unwrap();
        assert_eq!(ack.task_id, "dev", "invalid structured falls through to the text path");
    }

    #[test]
    fn no_signal_returns_none() {
        assert_eq!(resolve_ack(None, "just prose, no ack at all"), None);
        assert_eq!(resolve_ack(Some("not json"), "more prose"), None);
    }

    #[test]
    fn failed_and_blocked_statuses_parse() {
        assert_eq!(Ack::from_regex("ACK a failed").unwrap().status, AckStatus::Failed);
        assert_eq!(Ack::from_regex("ACK a blocked").unwrap().status, AckStatus::Blocked);
    }

    #[test]
    fn invalid_status_is_rejected() {
        assert!(Ack::from_json(r#"{"task_id":"a","status":"maybe"}"#).is_none());
        assert!(Ack::from_json(r#"{"status":"done"}"#).is_none(), "missing task_id");
    }

    #[test]
    fn multiple_lines_pick_the_matching_line() {
        let text = "ACK first blocked\nmore\nACK second done ship it\n";
        let ack = Ack::from_regex(text).unwrap();
        assert_eq!(ack.task_id, "first", "the first ACK line wins");
    }

    #[test]
    fn ack_roundtrips_through_json() {
        let ack = Ack {
            task_id: "dev".to_owned(),
            status: AckStatus::Done,
            summary: Some("done".to_owned()),
            approved: Some(true),
            needs_revision: None,
        };
        let back: Ack = serde_json::from_str(&serde_json::to_string(&ack).unwrap()).unwrap();
        assert_eq!(back, ack);
    }

    #[test]
    fn parse_rejects_bad_revision() {
        let err = Ack::parse(&serde_json::json!({
            "task_id": "a", "status": "done", "needs_revision": "huge"
        }));
        assert!(err.is_err());
    }
}
