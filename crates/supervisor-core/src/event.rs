//! The internal event bus model (§4.18).
//!
//! The daemon runs a `tokio::sync::broadcast` channel with a tagged event enum.
//! The core defines the enum so every crate shares the same wire shape; the
//! daemon owns the actual channel and the (un)subscription logic.
//!
//! Journaled topics: `Workflow`, `Inbox` (enqueue/deliver), `Fleet`, `Decision`.
//! Cheap `Signal`s are not journaled.

use serde::{Deserialize, Serialize};

use crate::dag::WorkflowEvent;
use crate::signal::Signal;
use crate::types::{DecisionRecord, InboxEntry, Workspace, WorkspaceState};

/// Every event published on the internal bus, tagged by topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "topic", rename_all = "snake_case")]
pub enum BusEvent {
    /// A raw external signal (SSE etc.). Not journaled.
    Signal(Signal),
    /// A workflow engine event. Journaled.
    Workflow(WorkflowEvent),
    /// An inbox mutation. Journaled.
    Inbox(InboxEvent),
    /// A fleet/workspace/agent state mutation. Journaled.
    Fleet(FleetEvent),
    /// A recorded decision. Journaled.
    Decision(DecisionRecord),
    /// Input from a human (CLI / API / slash command).
    Human(HumanEvent),
}

/// Inbox events (§4.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboxEvent {
    Enqueued { entry: InboxEntry },
    Delivered { id: String, delivered_at: String },
}

/// Fleet events: workspace lifecycle and agent state changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetEvent {
    WorkspaceState { workspace: Workspace },
    AgentState { workspace_id: String, agent_id: String, state: crate::types::AgentState },
}

impl FleetEvent {
    /// A coarse change descriptor for dashboards and wake-up logic.
    #[must_use]
    pub fn workspace_state(ws: Workspace) -> Self {
        Self::WorkspaceState { workspace: ws }
    }

    #[must_use]
    pub fn is_workspace(&self, id: &str, state: WorkspaceState) -> bool {
        matches!(self, Self::WorkspaceState { workspace } if workspace.id == id && workspace.state == state)
    }
}

/// Human input from the CLI, the API, or a slash command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HumanEvent {
    Command { command: String, args: Vec<String> },
    Feedback { workspace_id: String, agent_id: String, body: String },
    Approve { proposal_id: String },
    Reject { proposal_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::WorkflowEvent;
    use crate::types::AgentState;

    #[test]
    fn bus_events_roundtrip_through_json() {
        let e = BusEvent::Workflow(WorkflowEvent::NodeReady {
            graph: "feature_lifecycle".to_owned(),
            node: "dev".to_owned(),
        });
        let back: BusEvent = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
        assert!(matches!(back, BusEvent::Workflow(WorkflowEvent::NodeReady { .. })));
    }

    #[test]
    fn signal_events_are_tagged_signal() {
        let e = BusEvent::Signal(Signal::SessionIdle {
            ws: "iot".to_owned(),
            agent: "dev_01".to_owned(),
        });
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["topic"], "signal");
        assert_eq!(json["signal"], "session_idle");
    }

    #[test]
    fn fleet_workspace_state_matches() {
        let ws = Workspace {
            id: "iot".to_owned(),
            path: "/x".to_owned(),
            port: None,
            server_pid: None,
            state: WorkspaceState::On,
            cmux_ws: None,
            layout_path: None,
            updated_at: "t".to_owned(),
        };
        let e = FleetEvent::workspace_state(ws);
        assert!(e.is_workspace("iot", WorkspaceState::On));
        assert!(!e.is_workspace("iot", WorkspaceState::Off));
    }

    #[test]
    fn fleet_agent_state_roundtrips() {
        let e = BusEvent::Fleet(FleetEvent::AgentState {
            workspace_id: "iot".to_owned(),
            agent_id: "dev_01".to_owned(),
            state: AgentState::Working,
        });
        let back: BusEvent = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn ack_events_roundtrip() {
        use crate::ack::Ack;
        let e = BusEvent::Workflow(WorkflowEvent::Ack {
            graph: "feature_lifecycle".to_owned(),
            ack: Ack {
                task_id: "dev".to_owned(),
                status: crate::types::AckStatus::Done,
                summary: None,
                approved: None,
                needs_revision: None,
            },
        });
        let back: BusEvent = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
    }
}
