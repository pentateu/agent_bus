//! Agent lifecycle state: states, signals, and the transition table.
//!
//! This is the pure core of the orchestration state model (see
//! `docs/specs/2026-08-10-orchestration.md`). The observer in the orchestrator
//! turns cmux activity into [`Signal`]s; [`transition`] is the single place
//! states are allowed to change. Transitions not in the table are rejected —
//! an agent must not silently flip between unrelated states.
//!
//! Nothing here touches I/O or the clock; the whole decision surface is a pure
//! function and is exhaustively unit-tested.

use serde::{Deserialize, Serialize};

/// How much a state value is to be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// From an authoritative signal: a lifecycle hook, a process exit code, or
    /// the cmux socket. Safe to act on.
    #[default]
    Observed,
    /// From a heuristic over output (an error pattern, a prompt marker). Never
    /// sufficient alone for a costly action.
    Inferred,
}

/// The current condition of one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Never seen, or the surface cannot be read right now.
    #[default]
    Unknown,
    /// Surface created, agent booting.
    Spawning,
    /// Running a turn — output is moving.
    Working,
    /// Finished a turn with no pending background work.
    Idle,
    /// Finished a turn and is waiting for human/manager input to continue.
    WaitingInput,
    /// Waiting for a tool-permission approval.
    BlockedPermission,
    /// Crashed, exited non-zero, or matched error markers.
    Error,
}

/// An observed fact about an agent. Cheap, batched, and never interpreted
/// beyond what the transition table says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum Signal {
    /// Agent is actively running a turn.
    Working,
    /// Lifecycle hook: turn finished, nothing pending.
    Idle,
    /// Lifecycle hook: waiting for human input.
    NeedsInput,
    /// Lifecycle hook: waiting for a tool-permission approval.
    BlockedPermission,
    /// The surface's process ended. `Some(0)` is a clean exit; a non-zero code
    /// or a kill without a code is an error.
    Exit { code: Option<i32> },
    /// Output matched an error pattern. Always inferred; never authoritative.
    OutputError,
    /// A new turn started (typically after `Error` or `WaitingInput`).
    Recovery,
}

/// One permitted state change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub from: AgentState,
    pub to: AgentState,
    pub provenance: Provenance,
    /// 0–100. Observed transitions are 100; inferred ones are lower and must
    /// not drive costly actions alone.
    pub confidence: u8,
    /// Short human- and log-readable reason, e.g. `"process exited 1"`.
    pub reason: String,
}

/// The full confidence of an observed transition.
const OBSERVED_CONFIDENCE: u8 = 100;
/// The confidence of an inferred transition: trustworthy enough to show, never
/// enough to act on alone.
const INFERRED_CONFIDENCE: u8 = 60;

/// Apply a signal to the current state, if the table permits it.
///
/// Returns `None` for *no change* — either the signal is consistent with the
/// current state (`Working` while `Working`) or the transition is not in the
/// table and is therefore rejected. Rejected transitions are a bug in the
/// observer, not a thing to guess about; the orchestrator surfaces them.
#[must_use]
pub fn transition(current: AgentState, signal: Signal) -> Option<Transition> {
    transition_with_reason(current, signal).map(|(to, provenance, confidence, reason)| Transition {
        from: current,
        to,
        provenance,
        confidence,
        reason,
    })
}

/// An observed transition carries full confidence; an inferred one carries a
/// fraction, trustworthy enough to display but never to act on alone.
fn observed(to: AgentState, reason: impl Into<String>) -> (AgentState, Provenance, u8, String) {
    (to, Provenance::Observed, OBSERVED_CONFIDENCE, reason.into())
}

fn inferred(to: AgentState, reason: impl Into<String>) -> (AgentState, Provenance, u8, String) {
    (to, Provenance::Inferred, INFERRED_CONFIDENCE, reason.into())
}

/// The table itself: `(state, signal)` -> next state, provenance, confidence,
/// reason. Kept separate so the pure lookup is easy to read against the spec.
///
/// Arms with identical bodies are kept separate on purpose: this function is
/// the machine-readable spec of allowed transitions, and each `(state, signal)`
/// pair is a one-line entry. Collapsing identical arms would make a reader
/// hunt for what a given pair does, which is exactly the reading this table
/// is meant to serve.
#[allow(clippy::match_same_arms, clippy::too_many_lines)]
fn transition_with_reason(
    current: AgentState,
    signal: Signal,
) -> Option<(AgentState, Provenance, u8, String)> {
    let exit = || match signal {
        Signal::Exit { code: Some(0) } => Some(observed(AgentState::Idle, "process exited 0")),
        Signal::Exit { code: Some(code) } => {
            Some(observed(AgentState::Error, format!("process exited {code}")))
        }
        Signal::Exit { code: None } => {
            Some(observed(AgentState::Error, "process ended without an exit code"))
        }
        _ => None,
    };

    match (current, signal) {
        // Bootstrapping: any first real signal beats Unknown.
        (AgentState::Unknown, Signal::Working) => {
            Some(observed(AgentState::Working, "turn started"))
        }
        (AgentState::Unknown, Signal::Idle) => Some(observed(AgentState::Idle, "turn finished")),
        (AgentState::Unknown, Signal::NeedsInput) => {
            Some(observed(AgentState::WaitingInput, "awaiting input"))
        }
        (AgentState::Unknown, Signal::BlockedPermission) => {
            Some(observed(AgentState::BlockedPermission, "awaiting permission"))
        }
        (AgentState::Unknown, Signal::Exit { .. }) => exit(),
        (AgentState::Unknown, Signal::OutputError) => {
            Some(inferred(AgentState::Error, "error pattern in output"))
        }
        (AgentState::Unknown, Signal::Recovery) => Some(observed(AgentState::Working, "recovered")),

        (AgentState::Spawning, Signal::Working) => {
            Some(observed(AgentState::Working, "boot complete"))
        }
        (AgentState::Spawning, Signal::Idle) => Some(observed(AgentState::Idle, "booted and idle")),
        (AgentState::Spawning, Signal::Exit { .. }) => exit(),
        (AgentState::Spawning, Signal::OutputError) => {
            Some(inferred(AgentState::Error, "error pattern while spawning"))
        }
        (AgentState::Spawning, Signal::Recovery) => {
            Some(observed(AgentState::Working, "recovered while spawning"))
        }
        // Spawning + needs-input / blocked-permission stay in place: the agent
        // is still booting.
        (AgentState::Spawning, Signal::NeedsInput) => None,
        (AgentState::Spawning, Signal::BlockedPermission) => None,

        // Working can drop to a paused state, but only via its own signals.
        (AgentState::Working, Signal::Working) => None,
        (AgentState::Working, Signal::Idle) => Some(observed(AgentState::Idle, "turn finished")),
        (AgentState::Working, Signal::NeedsInput) => {
            Some(observed(AgentState::WaitingInput, "awaiting input"))
        }
        (AgentState::Working, Signal::BlockedPermission) => {
            Some(observed(AgentState::BlockedPermission, "awaiting permission"))
        }
        (AgentState::Working, Signal::Exit { .. }) => exit(),
        (AgentState::Working, Signal::OutputError) => {
            Some(inferred(AgentState::Error, "error pattern in output"))
        }
        (AgentState::Working, Signal::Recovery) => None,

        // Idle resumes on work; a clean exit while idle is a no-op, a crash is
        // an error.
        (AgentState::Idle, Signal::Working) => Some(observed(AgentState::Working, "turn started")),
        (AgentState::Idle, Signal::Idle) => None,
        (AgentState::Idle, Signal::NeedsInput) => {
            Some(observed(AgentState::WaitingInput, "awaiting input"))
        }
        // An idle agent has no turn running, so it cannot be blocked on a
        // permission prompt; that must come through Working first.
        (AgentState::Idle, Signal::BlockedPermission) => None,
        (AgentState::Idle, Signal::Exit { code: Some(0) }) => None,
        (AgentState::Idle, Signal::Exit { .. }) => exit(),
        (AgentState::Idle, Signal::OutputError) => {
            Some(inferred(AgentState::Error, "error pattern in output"))
        }
        (AgentState::Idle, Signal::Recovery) => Some(observed(AgentState::Working, "recovered")),

        // Waiting for input: input arriving is indistinguishable from a new
        // turn starting, so Recovery/Working both mean "got it, resuming".
        (AgentState::WaitingInput, Signal::Working) => {
            Some(observed(AgentState::Working, "input received"))
        }
        (AgentState::WaitingInput, Signal::Idle) => {
            Some(observed(AgentState::Idle, "no input needed after all"))
        }
        (AgentState::WaitingInput, Signal::NeedsInput) => None,
        (AgentState::WaitingInput, Signal::BlockedPermission) => {
            Some(observed(AgentState::BlockedPermission, "awaiting permission"))
        }
        (AgentState::WaitingInput, Signal::Recovery) => {
            Some(observed(AgentState::Working, "input received"))
        }
        (AgentState::WaitingInput, Signal::Exit { .. }) => exit(),
        (AgentState::WaitingInput, Signal::OutputError) => {
            Some(inferred(AgentState::Error, "error pattern in output"))
        }

        // Permission granted shows up as a new turn.
        (AgentState::BlockedPermission, Signal::Working) => {
            Some(observed(AgentState::Working, "permission granted"))
        }
        (AgentState::BlockedPermission, Signal::Idle) => {
            Some(observed(AgentState::Idle, "turn finished"))
        }
        (AgentState::BlockedPermission, Signal::Recovery) => {
            Some(observed(AgentState::Working, "permission granted"))
        }
        (AgentState::BlockedPermission, Signal::Exit { .. }) => exit(),
        // Blocked-permission + needs-input / output-error stay in place.
        (AgentState::BlockedPermission, Signal::NeedsInput) => None,
        (AgentState::BlockedPermission, Signal::BlockedPermission) => None,
        (AgentState::BlockedPermission, Signal::OutputError) => None,

        // Error: only Recovery or real work drags it out. A second error marker
        // is not a new transition — it is confirmation, but not a change.
        (AgentState::Error, Signal::Recovery) => Some(observed(AgentState::Working, "recovered")),
        (AgentState::Error, Signal::Working) => Some(observed(AgentState::Working, "recovered")),
        (AgentState::Error, Signal::Idle) => Some(observed(AgentState::Idle, "recovered and idle")),
        (AgentState::Error, Signal::NeedsInput) => {
            Some(observed(AgentState::WaitingInput, "awaiting input"))
        }
        (AgentState::Error, Signal::BlockedPermission) => {
            Some(observed(AgentState::BlockedPermission, "awaiting permission"))
        }
        (AgentState::Error, Signal::Exit { .. }) => None,
        (AgentState::Error, Signal::OutputError) => None,
    }
}

/// A state store row for one agent (see `docs/specs/2026-08-10-orchestration.md`,
/// *Decisions 3*). The orchestrator keeps these in an in-memory map keyed by
/// agent id and rebuilds them from an append-only state event log on start.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentRecord {
    pub agent: String,
    #[serde(default)]
    pub state: AgentState,
    #[serde(default)]
    pub provenance: Provenance,
    #[serde(default)]
    pub confidence: u8,
    /// The most recent terminal output snapshot (last N lines), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output: Option<String>,
    /// Error exits/markers in the current 1h window, for rerun-bound rules.
    #[serde(default)]
    pub error_count_1h: u32,
    /// How many addressed messages sit in the agent's inbox.
    #[serde(default)]
    pub inbox_depth: u64,
}

impl AgentRecord {
    #[must_use]
    pub fn new(agent: impl Into<String>) -> Self {
        Self { agent: agent.into(), ..Self::default() }
    }

    /// Apply a permitted transition, updating state, provenance, confidence,
    /// and the 1h error counter in one place.
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
    use super::*;

    #[test]
    fn unknown_becomes_working_on_first_signal() {
        let t = transition(AgentState::Unknown, Signal::Working).unwrap();
        assert_eq!((t.from, t.to), (AgentState::Unknown, AgentState::Working));
        assert_eq!(t.provenance, Provenance::Observed);
        assert_eq!(t.confidence, OBSERVED_CONFIDENCE);
    }

    #[test]
    fn exit_codes_map_to_idle_or_error() {
        let ok = transition(AgentState::Working, Signal::Exit { code: Some(0) }).unwrap();
        assert_eq!(ok.to, AgentState::Idle);
        assert_eq!(ok.provenance, Provenance::Observed);

        let boom = transition(AgentState::Working, Signal::Exit { code: Some(1) }).unwrap();
        assert_eq!(boom.to, AgentState::Error);
        assert_eq!(boom.provenance, Provenance::Observed);
        assert!(boom.reason.contains('1'), "reason names the exit code: {}", boom.reason);

        let killed = transition(AgentState::Working, Signal::Exit { code: None }).unwrap();
        assert_eq!(killed.to, AgentState::Error);
    }

    #[test]
    fn output_error_is_inferred_and_low_confidence() {
        let t = transition(AgentState::Working, Signal::OutputError).unwrap();
        assert_eq!(t.to, AgentState::Error);
        assert_eq!(t.provenance, Provenance::Inferred);
        assert!(t.confidence < OBSERVED_CONFIDENCE);
    }

    #[test]
    fn error_only_recovers_via_work() {
        let recovered = transition(AgentState::Error, Signal::Recovery).unwrap();
        assert_eq!(recovered.to, AgentState::Working);
        assert_eq!(transition(AgentState::Error, Signal::OutputError), None);
        assert_eq!(transition(AgentState::Error, Signal::Exit { code: Some(0) }), None);
    }

    #[test]
    fn consistent_signals_are_no_ops() {
        assert_eq!(transition(AgentState::Working, Signal::Working), None);
        assert_eq!(transition(AgentState::Idle, Signal::Idle), None);
        assert_eq!(transition(AgentState::WaitingInput, Signal::NeedsInput), None);
        assert_eq!(transition(AgentState::BlockedPermission, Signal::BlockedPermission), None);
    }

    /// Transitions outside the table are rejected, not guessed. An idle agent
    /// cannot suddenly be blocked on a permission; that must come through
    /// Working first.
    #[test]
    fn untabulated_transitions_are_rejected() {
        assert_eq!(transition(AgentState::Idle, Signal::BlockedPermission), None);
        assert_eq!(transition(AgentState::BlockedPermission, Signal::NeedsInput), None);
        assert_eq!(transition(AgentState::Idle, Signal::Recovery).unwrap().to, AgentState::Working);
    }

    #[test]
    fn waiting_input_resumes_on_work_or_recovery() {
        for signal in [Signal::Working, Signal::Recovery] {
            let t = transition(AgentState::WaitingInput, signal).unwrap();
            assert_eq!(t.to, AgentState::Working, "{signal:?} must resume work");
        }
    }

    #[test]
    fn permission_granted_reads_as_a_turn() {
        let t = transition(AgentState::BlockedPermission, Signal::Working).unwrap();
        assert_eq!(t.to, AgentState::Working);
    }

    #[test]
    fn applying_a_transition_updates_the_record() {
        let rec = AgentRecord::new("tester_01");
        let t = transition(AgentState::Unknown, Signal::Exit { code: Some(3) }).unwrap();
        let rec = rec.apply(&t);
        assert_eq!(rec.state, AgentState::Error);
        assert_eq!(rec.error_count_1h, 1);
        // A second error marker is not a new transition; a recovery is.
        let ok = transition(rec.state, Signal::Recovery).unwrap();
        let rec = rec.apply(&ok);
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.error_count_1h, 1, "error count only grows on error transitions");
    }

    #[test]
    fn agent_record_defaults_to_unknown() {
        let rec = AgentRecord::new("dev_01");
        assert_eq!(rec.state, AgentState::Unknown);
        assert_eq!(rec.provenance, Provenance::Observed);
        assert_eq!(rec.confidence, 0);
        assert_eq!(rec.inbox_depth, 0);
    }

    #[test]
    fn agent_record_roundtrips_through_json() {
        let rec = AgentRecord::new("dev_01").apply(&Transition {
            from: AgentState::Unknown,
            to: AgentState::Error,
            provenance: Provenance::Observed,
            confidence: 100,
            reason: "process exited 2".to_owned(),
        });
        let back: AgentRecord =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back, rec);
    }
}
