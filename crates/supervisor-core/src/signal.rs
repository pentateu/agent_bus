//! The external-signal model (C7) and the verified opencode event mapping.
//!
//! Every per-session opencode SSE frame carries its `sessionID`, so a signal
//! is always scoped to `(ws, agent)`. `server.heartbeat` is connection-scoped
//! and never maps to an agent (see §4.6 of the detailed design). The mapping
//! table below is the verified inventory from the design spec.

use serde::{Deserialize, Serialize};

use crate::types::SessionStatus;

/// A scoped fact observed about one agent, published on the internal bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum Signal {
    SessionStatus {
        ws: String,
        agent: String,
        status: SessionStatus,
    },
    SessionIdle {
        ws: String,
        agent: String,
    },
    StepStarted {
        ws: String,
        agent: String,
    },
    StepEnded {
        ws: String,
        agent: String,
    },
    StepFailed {
        ws: String,
        agent: String,
        error: Option<String>,
    },
    ToolFailed {
        ws: String,
        agent: String,
        name: String,
    },
    PermissionAsked {
        ws: String,
        agent: String,
        permission_id: String,
    },
    NeedsInput {
        ws: String,
        agent: String,
    },
    SessionError {
        ws: String,
        agent: String,
    },
    Diff {
        ws: String,
        agent: String,
    },
    /// Connection liveness only — carries no agent and never maps to one.
    Heartbeat {
        ws: String,
    },
}

impl Signal {
    /// The `(ws, agent)` scope, or `None` for connection-scoped heartbeats.
    #[must_use]
    pub fn scope(&self) -> Option<(&str, &str)> {
        match self {
            Self::SessionStatus { ws, agent, .. }
            | Self::SessionIdle { ws, agent }
            | Self::StepStarted { ws, agent }
            | Self::StepEnded { ws, agent }
            | Self::StepFailed { ws, agent, .. }
            | Self::ToolFailed { ws, agent, .. }
            | Self::PermissionAsked { ws, agent, .. }
            | Self::NeedsInput { ws, agent }
            | Self::SessionError { ws, agent }
            | Self::Diff { ws, agent } => Some((ws, agent)),
            Self::Heartbeat { .. } => None,
        }
    }

    /// Is this signal only about connection liveness?
    #[must_use]
    pub fn is_connection_scoped(&self) -> bool {
        matches!(self, Self::Heartbeat { .. })
    }

    /// The signal's name, matching the §4.6 inventory (used by rules' `signal`
    /// field and by bake-back).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionStatus { status: SessionStatus::Busy, .. } => "status.busy",
            Self::SessionStatus { status: SessionStatus::Retry, .. } => "status.retry",
            Self::SessionStatus { status: SessionStatus::Idle, .. } => "status.idle",
            Self::SessionIdle { .. } => "session.idle",
            Self::StepStarted { .. } => "step.started",
            Self::StepEnded { .. } => "step.ended",
            Self::StepFailed { .. } => "step.failed",
            Self::ToolFailed { .. } => "tool.failed",
            Self::PermissionAsked { .. } => "permission.asked",
            Self::NeedsInput { .. } => "needs_input",
            Self::SessionError { .. } => "session.error",
            Self::Diff { .. } => "session.diff",
            Self::Heartbeat { .. } => "server.heartbeat",
        }
    }
}

/// A raw opencode `/event` SSE frame, before mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpencodeEvent {
    /// The event type, e.g. `session.status`.
    pub event_type: String,
    /// The session id the frame is scoped to, if the frame carries one.
    pub session_id: Option<String>,
    /// The full frame body; fields are pulled out per the verified inventory.
    pub payload: serde_json::Value,
}

impl Signal {
    /// Map a raw opencode event to a scoped [`Signal`].
    ///
    /// `resolve(session_id)` yields the `(ws, agent)` for a session; unknown
    /// sessions are ignored. A connection-scoped heartbeat maps to
    /// `Heartbeat { ws }` using the workspace id directly.
    #[must_use]
    pub fn from_opencode(
        event: &OpencodeEvent,
        resolve: &dyn Fn(&str) -> Option<(String, String)>,
        ws: &str,
    ) -> Option<Self> {
        let scope = |sid: Option<&String>| -> Option<(String, String)> {
            match sid {
                Some(id) if !id.is_empty() => resolve(id),
                _ => None,
            }
        };

        match event.event_type.as_str() {
            "session.status" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                match session_status(&event.payload) {
                    Some("idle") => Some(Self::SessionIdle { ws, agent }),
                    Some("busy") => {
                        Some(Self::SessionStatus { ws, agent, status: SessionStatus::Busy })
                    }
                    Some("retry") => {
                        Some(Self::SessionStatus { ws, agent, status: SessionStatus::Retry })
                    }
                    _ => None,
                }
            }
            "session.idle" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                Some(Self::SessionIdle { ws, agent })
            }
            "session.next.step.started" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                Some(Self::StepStarted { ws, agent })
            }
            "session.next.step.ended" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                Some(Self::StepEnded { ws, agent })
            }
            "session.next.step.failed" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                let error = event
                    .payload
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                Some(Self::StepFailed { ws, agent, error })
            }
            "session.next.tool.failed" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                let name = event
                    .payload
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Some(Self::ToolFailed { ws, agent, name })
            }
            "permission.asked" | "message.part.updated" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                let permission_id = event
                    .payload
                    .get("permissionId")
                    .or_else(|| event.payload.get("permission_id"))
                    .or_else(|| event.payload.get("permission"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if permission_id.is_empty() {
                    return None;
                }
                Some(Self::PermissionAsked { ws, agent, permission_id })
            }
            "needs_input" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                Some(Self::NeedsInput { ws, agent })
            }
            "session.error" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                Some(Self::SessionError { ws, agent })
            }
            "session.diff" => {
                let (ws, agent) = scope(event.session_id.as_ref())?;
                Some(Self::Diff { ws, agent })
            }
            "server.heartbeat" | "server.connected" => Some(Self::Heartbeat { ws: ws.to_owned() }),
            _ => None,
        }
    }
}

/// Extract the status string from a `session.status` frame, tolerant of both
/// opencode shapes seen in the wild:
///
/// - v1.18.x (verified live 2026-08-14): the status is an object nested under
///   `properties`: `{"type":"session.status","properties":{"status":{"type":"busy"}}}`.
/// - v2 beta (the 2026-08-13 verified inventory): a top-level string:
///   `{"type":"session.status","status":"busy"}`.
#[must_use]
pub fn session_status(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("properties")
        .and_then(|p| p.get("status"))
        .and_then(|s| s.get("type"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| payload.get("status").and_then(serde_json::Value::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        event_type: &str,
        session_id: Option<&str>,
        payload: serde_json::Value,
    ) -> OpencodeEvent {
        OpencodeEvent {
            event_type: event_type.to_owned(),
            session_id: session_id.map(str::to_owned),
            payload,
        }
    }

    fn resolver(sid: &str) -> (String, String) {
        ("iot".to_owned(), format!("dev_{sid}"))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn resolve(sid: &str) -> Option<(String, String)> {
        Some(resolver(sid))
    }

    #[test]
    fn heartbeat_is_connection_scoped() {
        let ev = event("server.heartbeat", None, serde_json::json!({}));
        let s = Signal::from_opencode(&ev, &resolve, "iot").expect("heartbeat maps");
        assert_eq!(s, Signal::Heartbeat { ws: "iot".to_owned() });
        assert!(s.is_connection_scoped());
        assert_eq!(s.scope(), None, "heartbeat never maps to an agent");
    }

    #[test]
    fn session_status_maps_with_scope() {
        let ev = event("session.status", Some("s1"), serde_json::json!({"status": "busy"}));
        let s = Signal::from_opencode(&ev, &resolve, "iot").unwrap();
        assert_eq!(
            s,
            Signal::SessionStatus {
                ws: "iot".to_owned(),
                agent: "dev_s1".to_owned(),
                status: SessionStatus::Busy
            }
        );
    }

    #[test]
    fn idle_status_and_session_idle_map_to_idle_signal() {
        for (ty, payload) in [
            ("session.status", serde_json::json!({"status": "idle"})),
            ("session.idle", serde_json::json!({})),
        ] {
            let ev = event(ty, Some("s1"), payload);
            assert!(matches!(
                Signal::from_opencode(&ev, &resolve, "iot"),
                Some(Signal::SessionIdle { .. })
            ));
        }
    }

    #[test]
    fn session_status_reads_the_nested_v1_shape() {
        // opencode 1.18.x emits `properties.status` as an object; without this
        // the observer silently dropped every status signal and agents stayed
        // "spawning" forever (caught live on 2026-08-14).
        let ev = event(
            "session.status",
            Some("s1"),
            serde_json::json!({"properties": {"status": {"type": "busy"}}}),
        );
        assert!(matches!(
            Signal::from_opencode(&ev, &resolve, "iot"),
            Some(Signal::SessionStatus { status: SessionStatus::Busy, .. })
        ));
        let ev = event(
            "session.status",
            Some("s1"),
            serde_json::json!({"properties": {"status": {"type": "idle"}}}),
        );
        assert!(matches!(
            Signal::from_opencode(&ev, &resolve, "iot"),
            Some(Signal::SessionIdle { .. })
        ));
    }

    #[test]
    fn session_status_still_accepts_the_flat_v2_shape() {
        let ev = event("session.status", Some("s1"), serde_json::json!({"status": "busy"}));
        assert!(matches!(
            Signal::from_opencode(&ev, &resolve, "iot"),
            Some(Signal::SessionStatus { status: SessionStatus::Busy, .. })
        ));
    }

    #[test]
    fn session_status_helper_extracts_both_shapes() {
        assert_eq!(
            session_status(&serde_json::json!({"properties": {"status": {"type": "busy"}}})),
            Some("busy")
        );
        assert_eq!(session_status(&serde_json::json!({"status": "retry"})), Some("retry"));
        assert_eq!(session_status(&serde_json::json!({"properties": {}})), None);
    }

    #[test]
    fn unknown_session_is_ignored() {
        let ev = event("session.status", Some("ghost"), serde_json::json!({"status": "busy"}));
        let resolver = |_sid: &str| None;
        assert!(Signal::from_opencode(&ev, &resolver, "iot").is_none());
    }

    #[test]
    fn missing_session_id_is_ignored_for_session_events() {
        let ev = event("session.error", None, serde_json::json!({}));
        assert!(Signal::from_opencode(&ev, &resolve, "iot").is_none());
    }

    #[test]
    fn permission_asked_reads_the_permission_id() {
        let ev = event("permission.asked", Some("s1"), serde_json::json!({"permissionId": "p_9"}));
        let s = Signal::from_opencode(&ev, &resolve, "iot").unwrap();
        assert_eq!(
            s,
            Signal::PermissionAsked {
                ws: "iot".to_owned(),
                agent: "dev_s1".to_owned(),
                permission_id: "p_9".to_owned()
            }
        );
    }

    #[test]
    fn step_failed_carries_the_error() {
        let ev =
            event("session.next.step.failed", Some("s1"), serde_json::json!({"error": "boom"}));
        let s = Signal::from_opencode(&ev, &resolve, "iot").unwrap();
        assert!(matches!(
            s,
            Signal::StepFailed { error: Some(e), .. } if e == "boom"
        ));
    }

    #[test]
    fn unknown_event_types_are_ignored() {
        let ev = event("message.updated", Some("s1"), serde_json::json!({}));
        assert!(Signal::from_opencode(&ev, &resolve, "iot").is_none());
    }

    #[test]
    fn names_match_the_inventory() {
        assert_eq!(Signal::Heartbeat { ws: "w".to_owned() }.name(), "server.heartbeat");
        assert_eq!(
            Signal::StepFailed { ws: "w".to_owned(), agent: "a".to_owned(), error: None }.name(),
            "step.failed"
        );
        assert_eq!(
            Signal::PermissionAsked {
                ws: "w".to_owned(),
                agent: "a".to_owned(),
                permission_id: "p".to_owned()
            }
            .name(),
            "permission.asked"
        );
    }

    #[test]
    fn signals_roundtrip_through_json() {
        let s = Signal::ToolFailed {
            ws: "iot".to_owned(),
            agent: "tester_01".to_owned(),
            name: "bash".to_owned(),
        };
        let back: Signal = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
