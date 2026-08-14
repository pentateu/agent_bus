//! The agent lifecycle state machine (§8).
//!
//! States are `unknown | spawning | working | idle | waiting_input |
//! blocked_permission | error` (the `agent.state` enum from the `SQLite` schema;
//! `recovery` is a *signal* — a new turn starting — not a state). The machine
//! consumes the scoped [`Signal`]s from §4.6, reducing each to a transition or
//! a no-op. Transitions not in the table are rejected and logged: an agent must
//! not silently flip between unrelated states.

use serde::{Deserialize, Serialize};

use crate::signal::Signal;
use crate::types::{AgentState, SessionStatus};

/// How much a state value is to be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// From an authoritative signal (SSE, process exit code). Safe to act on.
    #[default]
    Observed,
    /// From a heuristic (an error pattern in output). Never sufficient alone
    /// for a costly action.
    Inferred,
}

/// One permitted state change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub from: AgentState,
    pub to: AgentState,
    pub provenance: Provenance,
    /// 0–1. Observed transitions are 1.0; inferred ones are lower and must not
    /// drive costly actions alone.
    pub confidence: f64,
    /// Short human- and log-readable reason, e.g. `"step.failed"`.
    pub reason: String,
}

/// The confidence of an observed transition.
const OBSERVED_CONFIDENCE: f64 = 1.0;
/// The confidence of an inferred transition (e.g. a tool failure pattern).
const INFERRED_CONFIDENCE: f64 = 0.6;

/// Reduce a scoped [`Signal`] to the state-machine action it stands for.
///
/// Informational signals (`Diff`, `Heartbeat`, a `retry` status) carry no
/// state change and map to `None`.
fn machine_action(signal: &Signal) -> Option<MachineSignal> {
    match signal {
        Signal::SessionStatus { status: SessionStatus::Busy, .. }
        | Signal::SessionStatus { status: SessionStatus::Retry, .. }
        | Signal::StepStarted { .. } => Some(MachineSignal::Working),
        Signal::SessionIdle { .. } | Signal::SessionStatus { status: SessionStatus::Idle, .. } => {
            Some(MachineSignal::Idle)
        }
        Signal::NeedsInput { .. } => Some(MachineSignal::NeedsInput),
        Signal::PermissionAsked { .. } => Some(MachineSignal::BlockedPermission),
        Signal::StepFailed { .. } | Signal::SessionError { .. } => {
            Some(MachineSignal::Failure { observed: true })
        }
        Signal::ToolFailed { .. } => Some(MachineSignal::Failure { observed: false }),
        // M2 deviation (§8): a step boundary is not a turn boundary. Idle is
        // only `session.idle` / `status: idle`, so a multi-step turn does not
        // flicker Working→Idle→Working. `StepEnded` is informational.
        Signal::StepEnded { .. } | Signal::Diff { .. } | Signal::Heartbeat { .. } => None,
    }
}

/// The reduced signal vocabulary of the §8 machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MachineSignal {
    Working,
    Idle,
    NeedsInput,
    BlockedPermission,
    /// A turn failure. `observed` marks authoritative failures (step failed,
    /// session error) vs. inferred markers (tool.failed).
    Failure {
        observed: bool,
    },
}

/// Apply a scoped signal to the current state, if the table permits it.
///
/// Returns `None` for *no change* — a signal consistent with the current state
/// or one not in the table. Rejected transitions are surfaced by the caller.
#[must_use]
pub fn transition(current: AgentState, signal: &Signal) -> Option<Transition> {
    let action = machine_action(signal)?;
    transition_action(current, action).map(|(to, provenance, confidence, reason)| Transition {
        from: current,
        to,
        provenance,
        confidence,
        reason,
    })
}

fn observed(to: AgentState, reason: &str) -> (AgentState, Provenance, f64, String) {
    (to, Provenance::Observed, OBSERVED_CONFIDENCE, reason.to_owned())
}

fn inferred(to: AgentState, reason: &str) -> (AgentState, Provenance, f64, String) {
    (to, Provenance::Inferred, INFERRED_CONFIDENCE, reason.to_owned())
}

/// The table itself: `(state, action)` -> next state, provenance, confidence,
/// reason. Kept as one match so it reads as the machine-readable spec.
#[allow(clippy::match_same_arms, clippy::too_many_lines)]
fn transition_action(
    current: AgentState,
    signal: MachineSignal,
) -> Option<(AgentState, Provenance, f64, String)> {
    let failure = |signal: MachineSignal| match signal {
        MachineSignal::Failure { observed: true } => {
            Some(observed(AgentState::Error, "turn step failed"))
        }
        MachineSignal::Failure { observed: false } => {
            Some(inferred(AgentState::Error, "tool failed marker"))
        }
        _ => None,
    };

    match (current, signal) {
        // Bootstrapping: any first real signal beats Unknown.
        (AgentState::Unknown, MachineSignal::Working) => {
            Some(observed(AgentState::Working, "turn started"))
        }
        (AgentState::Unknown, MachineSignal::Idle) => {
            Some(observed(AgentState::Idle, "turn finished"))
        }
        (AgentState::Unknown, MachineSignal::NeedsInput) => {
            Some(observed(AgentState::WaitingInput, "awaiting input"))
        }
        (AgentState::Unknown, MachineSignal::BlockedPermission) => {
            Some(observed(AgentState::BlockedPermission, "awaiting permission"))
        }
        (AgentState::Unknown, MachineSignal::Failure { .. }) => failure(signal),

        (AgentState::Spawning, MachineSignal::Working) => {
            Some(observed(AgentState::Working, "boot complete"))
        }
        (AgentState::Spawning, MachineSignal::Idle) => {
            Some(observed(AgentState::Idle, "booted and idle"))
        }
        (AgentState::Spawning, MachineSignal::Failure { .. }) => failure(signal),
        // Spawning + needs-input / blocked-permission stay in place: the agent
        // is still booting.
        (AgentState::Spawning, MachineSignal::NeedsInput) => None,
        (AgentState::Spawning, MachineSignal::BlockedPermission) => None,

        // Working can drop to a paused state, but only via its own signals.
        (AgentState::Working, MachineSignal::Working) => None,
        (AgentState::Working, MachineSignal::Idle) => {
            Some(observed(AgentState::Idle, "turn finished"))
        }
        (AgentState::Working, MachineSignal::NeedsInput) => {
            Some(observed(AgentState::WaitingInput, "awaiting input"))
        }
        (AgentState::Working, MachineSignal::BlockedPermission) => {
            Some(observed(AgentState::BlockedPermission, "awaiting permission"))
        }
        (AgentState::Working, MachineSignal::Failure { .. }) => failure(signal),

        // Idle resumes on work; an idle agent has no turn to fail.
        (AgentState::Idle, MachineSignal::Working) => {
            Some(observed(AgentState::Working, "turn started"))
        }
        (AgentState::Idle, MachineSignal::Idle) => None,
        (AgentState::Idle, MachineSignal::NeedsInput) => {
            Some(observed(AgentState::WaitingInput, "awaiting input"))
        }
        (AgentState::Idle, MachineSignal::BlockedPermission) => None,
        (AgentState::Idle, MachineSignal::Failure { .. }) => failure(signal),

        // Waiting for input: new work means "got it, resuming".
        (AgentState::WaitingInput, MachineSignal::Working) => {
            Some(observed(AgentState::Working, "input received"))
        }
        (AgentState::WaitingInput, MachineSignal::Idle) => {
            Some(observed(AgentState::Idle, "no input needed after all"))
        }
        (AgentState::WaitingInput, MachineSignal::NeedsInput) => None,
        (AgentState::WaitingInput, MachineSignal::BlockedPermission) => {
            Some(observed(AgentState::BlockedPermission, "awaiting permission"))
        }
        (AgentState::WaitingInput, MachineSignal::Failure { .. }) => failure(signal),

        // Permission granted shows up as a new turn.
        (AgentState::BlockedPermission, MachineSignal::Working) => {
            Some(observed(AgentState::Working, "permission granted"))
        }
        (AgentState::BlockedPermission, MachineSignal::Idle) => {
            Some(observed(AgentState::Idle, "turn finished"))
        }
        (AgentState::BlockedPermission, MachineSignal::Failure { .. }) => failure(signal),
        // Blocked-permission + needs-input stays in place.
        (AgentState::BlockedPermission, MachineSignal::NeedsInput) => None,
        (AgentState::BlockedPermission, MachineSignal::BlockedPermission) => None,

        // Error: only real work drags it out. A second failure marker is not a
        // new transition — confirmation, not a change.
        (AgentState::Error, MachineSignal::Working) => {
            Some(observed(AgentState::Working, "recovered"))
        }
        (AgentState::Error, MachineSignal::Idle) => {
            Some(observed(AgentState::Idle, "recovered and idle"))
        }
        (AgentState::Error, MachineSignal::NeedsInput) => {
            Some(observed(AgentState::WaitingInput, "awaiting input"))
        }
        (AgentState::Error, MachineSignal::BlockedPermission) => {
            Some(observed(AgentState::BlockedPermission, "awaiting permission"))
        }
        (AgentState::Error, MachineSignal::Failure { .. }) => None,
    }
}

/// A state-store row for one agent, mirroring the `agent` table. Rebuilt from
/// the journal on start.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRecord {
    pub ws: String,
    pub agent: String,
    #[serde(default)]
    pub state: AgentState,
    #[serde(default)]
    pub provenance: Provenance,
    #[serde(default = "default_one_f64")]
    pub confidence: f64,
    /// The most recent terminal output snapshot, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output: Option<String>,
    /// Error transitions in the current rolling window, for rerun-bound rules.
    #[serde(default)]
    pub error_count_1h: u32,
}

fn default_one_f64() -> f64 {
    1.0
}

impl AgentRecord {
    #[must_use]
    pub fn new(ws: impl Into<String>, agent: impl Into<String>) -> Self {
        Self { ws: ws.into(), agent: agent.into(), ..Self::default() }
    }

    /// Apply a permitted transition, updating state, provenance, confidence,
    /// and the error counter in one place.
    #[must_use]
    pub fn apply(mut self, t: &Transition) -> Self {
        self.state = t.to;
        self.provenance = t.provenance;
        self.confidence = t.confidence;
        if t.to == AgentState::Error {
            self.error_count_1h = self.error_count_1h.saturating_add(1);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::float_cmp,
        clippy::match_single_binding,
        clippy::wildcard_in_or_patterns,
        clippy::cast_precision_loss
    )]
    use super::*;

    fn sig(s: Signal) -> Signal {
        s
    }

    fn status(status: SessionStatus) -> Signal {
        Signal::SessionStatus { ws: "w".to_owned(), agent: "a".to_owned(), status }
    }

    fn idle() -> Signal {
        Signal::SessionIdle { ws: "w".to_owned(), agent: "a".to_owned() }
    }

    fn step_failed() -> Signal {
        Signal::StepFailed { ws: "w".to_owned(), agent: "a".to_owned(), error: None }
    }

    fn tool_failed() -> Signal {
        Signal::ToolFailed { ws: "w".to_owned(), agent: "a".to_owned(), name: "bash".to_owned() }
    }

    fn permission() -> Signal {
        Signal::PermissionAsked {
            ws: "w".to_owned(),
            agent: "a".to_owned(),
            permission_id: "p".to_owned(),
        }
    }

    #[test]
    fn unknown_becomes_working_on_first_work_signal() {
        let t = transition(AgentState::Unknown, &status(SessionStatus::Busy)).unwrap();
        assert_eq!((t.from, t.to), (AgentState::Unknown, AgentState::Working));
        assert_eq!(t.provenance, Provenance::Observed);
        assert_eq!(t.confidence, OBSERVED_CONFIDENCE);
    }

    #[test]
    fn step_started_marks_working() {
        let t = transition(
            AgentState::Idle,
            &sig(Signal::StepStarted { ws: "w".to_owned(), agent: "a".to_owned() }),
        )
        .unwrap();
        assert_eq!(t.to, AgentState::Working);
    }

    #[test]
    fn session_idle_marks_idle() {
        for s in [idle(), status(SessionStatus::Idle)] {
            let t = transition(AgentState::Working, &s).unwrap();
            assert_eq!(t.to, AgentState::Idle);
            assert_eq!(t.provenance, Provenance::Observed);
        }
    }

    #[test]
    fn needs_input_and_permission_pause_the_agent() {
        let needs = transition(
            AgentState::Working,
            &sig(Signal::NeedsInput { ws: "w".to_owned(), agent: "a".to_owned() }),
        )
        .unwrap();
        assert_eq!(needs.to, AgentState::WaitingInput);
        let perm = transition(AgentState::Working, &permission()).unwrap();
        assert_eq!(perm.to, AgentState::BlockedPermission);
    }

    #[test]
    fn observed_failures_are_authoritative() {
        let t = transition(AgentState::Working, &step_failed()).unwrap();
        assert_eq!(t.to, AgentState::Error);
        assert_eq!(t.provenance, Provenance::Observed);
        assert_eq!(t.confidence, OBSERVED_CONFIDENCE);
    }

    #[test]
    fn tool_failed_is_inferred_and_low_confidence() {
        let t = transition(AgentState::Working, &tool_failed()).unwrap();
        assert_eq!(t.to, AgentState::Error);
        assert_eq!(t.provenance, Provenance::Inferred);
        assert!(t.confidence < OBSERVED_CONFIDENCE);
    }

    #[test]
    fn error_recovers_only_via_real_work() {
        let recovered = transition(AgentState::Error, &status(SessionStatus::Busy)).unwrap();
        assert_eq!(recovered.to, AgentState::Working);
        assert_eq!(recovered.reason, "recovered");
        assert_eq!(transition(AgentState::Error, &step_failed()), None);
    }

    #[test]
    fn consistent_signals_are_no_ops() {
        assert_eq!(transition(AgentState::Working, &status(SessionStatus::Busy)), None);
        assert_eq!(transition(AgentState::Idle, &idle()), None);
        assert_eq!(
            transition(
                AgentState::WaitingInput,
                &sig(Signal::NeedsInput { ws: "w".to_owned(), agent: "a".to_owned() })
            ),
            None
        );
        assert_eq!(transition(AgentState::BlockedPermission, &permission()), None);
    }

    #[test]
    fn informational_signals_never_change_state() {
        let diff = Signal::Diff { ws: "w".to_owned(), agent: "a".to_owned() };
        let heartbeat = Signal::Heartbeat { ws: "w".to_owned() };
        let step_ended = Signal::StepEnded { ws: "w".to_owned(), agent: "a".to_owned() };
        assert_eq!(transition(AgentState::Working, &diff), None);
        assert_eq!(transition(AgentState::Working, &heartbeat), None);
        // M2 deviation: a step boundary is not a turn boundary.
        assert_eq!(transition(AgentState::Working, &step_ended), None);
        assert_eq!(transition(AgentState::Working, &idle()).unwrap().to, AgentState::Idle);
    }

    #[test]
    fn untabulated_transitions_are_rejected() {
        assert_eq!(transition(AgentState::Idle, &permission()), None);
        assert_eq!(
            transition(
                AgentState::BlockedPermission,
                &sig(Signal::NeedsInput { ws: "w".to_owned(), agent: "a".to_owned() })
            ),
            None
        );
    }

    #[test]
    fn waiting_input_resumes_on_work() {
        let t = transition(AgentState::WaitingInput, &status(SessionStatus::Busy)).unwrap();
        assert_eq!(t.to, AgentState::Working);
    }

    #[test]
    fn applying_a_transition_updates_the_record() {
        let rec = AgentRecord::new("iot", "tester_01");
        let t = transition(AgentState::Unknown, &step_failed()).unwrap();
        let rec = rec.apply(&t);
        assert_eq!(rec.state, AgentState::Error);
        assert_eq!(rec.error_count_1h, 1);
        let ok = transition(rec.state, &status(SessionStatus::Busy)).unwrap();
        let rec = rec.apply(&ok);
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.error_count_1h, 1, "error count only grows on error transitions");
    }

    #[test]
    fn record_roundtrips_through_json() {
        let rec = AgentRecord::new("iot", "dev_01").apply(&Transition {
            from: AgentState::Unknown,
            to: AgentState::Error,
            provenance: Provenance::Observed,
            confidence: 1.0,
            reason: "step.failed".to_owned(),
        });
        let back: AgentRecord =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back, rec);
    }
}
