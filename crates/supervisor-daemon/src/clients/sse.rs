//! The SSE observer (C7): one task per `on` workspace streaming `GET /event`
//! and publishing scoped [`Signal`]s on the internal bus.
//!
//! Reconnect: on EOF/error, exponential backoff (1s, 2s, 4s, … max 60s).
//! `server.heartbeat` frames reset a watchdog; if no heartbeat for 90s the
//! connection is force-reconnected. Each frame is mapped via
//! [`supervisor_core::signal::Signal::from_opencode`], so a session-less
//! heartbeat never maps to an agent.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use supervisor_core::event::BusEvent;
use supervisor_core::signal::{OpencodeEvent, Signal};
use supervisor_core::types::AgentId;
use tokio_util::sync::CancellationToken;

use crate::bus::SharedBus;
use crate::clients::opencode::OpencodeClient;

/// Max time without a heartbeat before forcing a reconnect.
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 90;
/// Reconnect backoff bounds.
pub const BACKOFF_MIN_SECS: u64 = 1;
pub const BACKOFF_MAX_SECS: u64 = 60;

/// One parsed SSE frame (event type + joined data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event_type: String,
    pub data: String,
}

/// Pure SSE parsing: `event:` / `data:` lines accumulate; a blank line
/// dispatches a frame; comment lines (`:`) are dropped. The opencode stream
/// also carries the event type in the JSON payload, so `data` is preserved
/// verbatim.
#[must_use]
pub fn parse_sse(input: &str) -> Vec<SseFrame> {
    let mut frames = Vec::new();
    let mut parser = SseParser::default();
    for line in input.lines() {
        if let Some(frame) = parser.push_line(line) {
            frames.push(frame);
        }
    }
    if let Some(frame) = parser.finish() {
        frames.push(frame);
    }
    frames
}

/// Incremental SSE frame assembler (used by the observer's line loop).
#[derive(Debug, Default)]
pub struct SseParser {
    event_type: String,
    data: Vec<String>,
    saw_field: bool,
}

impl SseParser {
    /// Feed one line. Blank lines dispatch a frame.
    pub fn push_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            if self.saw_field {
                let frame = SseFrame {
                    event_type: std::mem::take(&mut self.event_type),
                    data: std::mem::take(&mut self.data).join("\n"),
                };
                self.saw_field = false;
                return Some(frame);
            }
            return None;
        }
        if let Some(value) = line.strip_prefix(':') {
            let _ = value;
            return None;
        }
        if let Some(value) = line.strip_prefix("event:") {
            value.trim().clone_into(&mut self.event_type);
            self.saw_field = true;
        } else if let Some(value) = line.strip_prefix("data:") {
            self.data.push(value.trim().to_owned());
            self.saw_field = true;
        }
        None
    }

    /// Flush any pending frame (trailing event with no closing blank line).
    pub fn finish(&mut self) -> Option<SseFrame> {
        if self.saw_field {
            self.saw_field = false;
            return Some(SseFrame {
                event_type: std::mem::take(&mut self.event_type),
                data: std::mem::take(&mut self.data).join("\n"),
            });
        }
        None
    }
}

/// The event type, prefering the SSE `event:` line and falling back to the
/// payload's `type` field (opencode puts the type in the payload).
#[must_use]
pub fn frame_event_type(frame: &SseFrame) -> String {
    if !frame.event_type.is_empty() {
        return frame.event_type.clone();
    }
    let value: serde_json::Value = serde_json::from_str(&frame.data).unwrap_or_default();
    value.get("type").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned()
}

/// Map a frame to a scoped signal.
#[must_use]
pub fn frame_to_signal(
    frame: &SseFrame,
    resolve: &dyn Fn(&str) -> Option<(String, String)>,
    ws: &str,
) -> Option<Signal> {
    let event = OpencodeEvent {
        event_type: frame_event_type(frame),
        session_id: extract_session_id(frame),
        payload: serde_json::from_str(&frame.data).unwrap_or_default(),
    };
    Signal::from_opencode(&event, resolve, ws)
}

/// Extract the session id from a frame's payload (`properties.sessionID` or
/// `data.sessionID`), per the verified inventory.
fn extract_session_id(frame: &SseFrame) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&frame.data).ok()?;
    value
        .get("properties")
        .and_then(|p| p.get("sessionID"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value.get("data").and_then(|d| d.get("sessionID")).and_then(serde_json::Value::as_str)
        })
        .or_else(|| value.get("sessionID").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

/// A session→agent resolver.
pub type SessionResolver = Arc<dyn Fn(&str) -> Option<(String, AgentId)> + Send + Sync>;

/// The running SSE observer task.
pub struct SseObserver {
    client: OpencodeClient,
    ws: String,
    resolve: SessionResolver,
    bus: SharedBus,
    shutdown: CancellationToken,
}

impl SseObserver {
    /// Build an observer for `ws` streaming from `client`.
    #[must_use]
    pub fn new(
        client: OpencodeClient,
        ws: String,
        resolve: SessionResolver,
        bus: SharedBus,
        shutdown: CancellationToken,
    ) -> Self {
        Self { client, ws, resolve, bus, shutdown }
    }

    /// Run until shutdown, reconnecting with backoff.
    pub async fn run(mut self) {
        let mut backoff = Duration::from_secs(BACKOFF_MIN_SECS);
        loop {
            if self.shutdown.is_cancelled() {
                tracing::info!(ws = %self.ws, "sse observer shutting down");
                return;
            }
            self.stream_once(&mut backoff).await;
            // stream_once returns on connection drop / error / heartbeat
            // timeout; sleep the backoff before reconnecting.
            tokio::select! {
                () = self.shutdown.cancelled() => {
                    tracing::info!(ws = %self.ws, "sse observer shutting down");
                    return;
                }
                () = tokio::time::sleep(backoff) => {}
            }
        }
    }

    async fn stream_once(&mut self, backoff: &mut Duration) {
        let url = match self.client.base_url().join("/event") {
            Ok(url) => url,
            Err(e) => {
                tracing::error!(ws = %self.ws, error = %e, "cannot join /event url");
                return;
            }
        };
        let res = match self.client.http_client().get(url).send().await {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!(ws = %self.ws, error = %e, "sse connect failed; reconnecting");
                *backoff = grow_backoff(*backoff);
                return;
            }
        };
        if !res.status().is_success() {
            tracing::warn!(ws = %self.ws, status = %res.status(), "sse non-200; reconnecting");
            *backoff = grow_backoff(*backoff);
            return;
        }
        *backoff = Duration::from_secs(BACKOFF_MIN_SECS);
        tracing::info!(ws = %self.ws, "sse observer connected");

        let mut stream = res.bytes_stream();
        let mut buf = Vec::new();
        let mut parser = SseParser::default();
        let mut last_heartbeat = tokio::time::Instant::now();
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                () = tokio::time::sleep(Duration::from_secs(HEARTBEAT_TIMEOUT_SECS)) => {
                    if last_heartbeat.elapsed() > Duration::from_secs(HEARTBEAT_TIMEOUT_SECS) {
                        tracing::warn!(ws = %self.ws, "sse heartbeat timeout; forcing reconnect");
                        return;
                    }
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            buf.extend_from_slice(&bytes);
                            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                let line = String::from_utf8_lossy(&buf[..nl]).into_owned();
                                buf.drain(..=nl);
                                self.handle_line(&mut parser, &line, &mut last_heartbeat);
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(ws = %self.ws, error = %e, "sse stream error; reconnecting");
                            return;
                        }
                        None => {
                            tracing::warn!(ws = %self.ws, "sse stream closed; reconnecting");
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Feed one line into the frame parser and dispatch a completed frame.
    fn handle_line(
        &self,
        parser: &mut SseParser,
        line: &str,
        last_heartbeat: &mut tokio::time::Instant,
    ) {
        if let Some(frame) = parser.push_line(line) {
            let event_type = frame_event_type(&frame);
            if event_type.contains("heartbeat") {
                *last_heartbeat = tokio::time::Instant::now();
            }
            let resolve = &|sid: &str| (self.resolve)(sid);
            if let Some(signal) = frame_to_signal(&frame, resolve, &self.ws) {
                self.bus.publish(BusEvent::Signal(signal));
            } else {
                // A frame arrived but produced no signal: unknown event type,
                // an unmapped session, or a session-less frame. Unmapped
                // *sessions* are the classic silent failure (a stale resolver
                // index) — debug-log it so the stream's health is inspectable
                // without guessing.
                let session = extract_session_id(&frame);
                tracing::debug!(
                    ws = %self.ws,
                    event = %event_type,
                    session = session.as_deref().unwrap_or("(none)"),
                    "sse frame produced no signal"
                );
            }
        }
    }
}

fn grow_backoff(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(BACKOFF_MAX_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_and_data_frames() {
        let input =
            "event: session.idle\ndata: {\"type\":\"session.idle\",\"sessionID\":\"s1\"}\n\n";
        let frames = parse_sse(input);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_type, "session.idle");
        assert!(frames[0].data.contains("sessionID"));
    }

    #[test]
    fn blank_lines_dispatch_and_comments_are_dropped() {
        let input = ": comment\n\nevent: server.heartbeat\ndata: {}\n\n";
        let frames = parse_sse(input);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_type, "server.heartbeat");
    }

    #[test]
    fn trailing_frame_without_blank_line_flushes() {
        let frames = parse_sse("data: {\"type\":\"x\"}\n");
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn multi_line_data_is_joined() {
        let frames = parse_sse("data: one\ndata: two\n\n");
        assert_eq!(frames[0].data, "one\ntwo");
    }

    #[test]
    fn incremental_parser_assembles_frames() {
        let mut parser = SseParser::default();
        assert!(parser.push_line("event: session.idle").is_none());
        assert!(parser.push_line(r#"data: {"type":"session.idle"}"#).is_none());
        let frame = parser.push_line("").expect("blank line dispatches");
        assert_eq!(frame.event_type, "session.idle");
        assert!(parser.push_line("").is_none(), "no pending fields → no frame");
        assert_eq!(parser.finish(), None);
    }

    #[test]
    fn frame_event_type_falls_back_to_payload() {
        let frame = SseFrame {
            event_type: String::new(),
            data: r#"{"type":"server.heartbeat"}"#.to_owned(),
        };
        assert_eq!(frame_event_type(&frame), "server.heartbeat");
    }

    #[test]
    fn session_id_extraction_from_properties() {
        let frame = SseFrame {
            event_type: "session.next.step.started".to_owned(),
            data: r#"{"properties":{"sessionID":"abc"}}"#.to_owned(),
        };
        let ev = OpencodeEvent {
            event_type: frame_event_type(&frame),
            session_id: extract_session_id(&frame),
            payload: serde_json::from_str(&frame.data).unwrap(),
        };
        assert_eq!(ev.session_id.as_deref(), Some("abc"));
        let resolve = |sid: &str| {
            if sid == "abc" { Some(("iot".to_owned(), "dev_01".to_owned())) } else { None }
        };
        let signal = Signal::from_opencode(&ev, &resolve, "iot");
        assert!(
            matches!(signal, Some(Signal::StepStarted { ws, agent }) if ws == "iot" && agent == "dev_01")
        );
    }

    #[test]
    fn heartbeat_never_maps_to_an_agent() {
        let frame = SseFrame {
            event_type: String::new(),
            data: r#"{"type":"server.heartbeat"}"#.to_owned(),
        };
        let resolve = |_: &str| Some(("iot".to_owned(), "dev_01".to_owned()));
        let signal = frame_to_signal(&frame, &resolve, "iot");
        assert_eq!(signal, Some(Signal::Heartbeat { ws: "iot".to_owned() }));
        assert!(signal.unwrap().scope().is_none());
    }

    #[test]
    fn unknown_session_is_ignored() {
        let frame = SseFrame {
            event_type: "session.idle".to_owned(),
            data: r#"{"sessionID":"ghost"}"#.to_owned(),
        };
        let resolve = |_: &str| None;
        assert_eq!(frame_to_signal(&frame, &resolve, "iot"), None);
    }
}
