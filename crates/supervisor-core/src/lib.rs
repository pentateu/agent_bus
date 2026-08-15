//! Pure domain logic for the agent-bus Fleet Supervisor (Orchestration v2).
//!
//! This crate contains everything that can be reasoned about without touching
//! a socket, process, or file: types, port math, the agent state machine, the
//! rule engine, the DAG engine, the layered ACK resolver, the journal model,
//! bake-back, the event model, and the config file shapes. The daemon crate
//! owns all I/O and async wiring and builds on these pure pieces.
//!
//! Source of truth: `docs/specs/2026-08-13-supervisor-detailed-design.md`.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod ack;
pub mod bakeback;
pub mod config;
pub mod dag;
pub mod error;
pub mod event;
pub mod graphs;
pub mod journal;
pub mod ports;
pub mod rules;
pub mod signal;
pub mod state;
pub mod time;
pub mod types;

pub use ack::{Ack, resolve_ack};
pub use bakeback::{
    Cluster, cluster, expire, normalized_signature, observed_success_rate, propose, resolve,
};
pub use config::{
    AutoApprove, GithubAdapterConfig, PortSetting, ProjectConfig, SupervisorConfig,
    default_project_toml,
};
pub use dag::{
    GraphDef, LoopBack, ManagerRuling, NodeDef, OnError, RoleResolution, RosterEntry, Workflow,
    WorkflowEvent,
};
pub use error::{CoreError, CoreResult};
pub use event::{BusEvent, FleetEvent, HumanEvent, InboxEvent};
pub use graphs::{BUG_FLOW_JSON, FEATURE_LIFECYCLE_JSON, default_graph, default_graph_ids};
pub use journal::{
    AgentStateEvent, InboxDeliverEvent, JournalRecord, JournalReplay, JournalType,
    WorkflowTransitionEvent, replay, replay_dedup,
};
pub use ports::{
    DEFAULT_API_PORT, DEFAULT_PORT_RANGE, DEFAULT_RESERVED_PORTS, DEFAULT_SUPERVISOR_PORT,
    PortAllocator, PortError, port_error_message,
};
pub use rules::{
    Action, Candidate, Cmp, CodeRule, Condition, CounterStore, DEFAULT_THRESHOLD, Decision,
    Evaluation, EventKind, NodeRef, Rule, RuleEngine, Situation, StrCmp,
};
pub use signal::{OpencodeEvent, Signal};
pub use state::{AgentRecord, Provenance, Transition, transition};
pub use time::now_rfc3339;
pub use types::{
    AckStatus, Agent, AgentId, AgentMode, AgentState, DecisionRecord, DriverKind, Graph,
    InboxEntry, IntakeItem, NodeState, NodeStateRow, PortRow, Priority, Proposal, ProposalStatus,
    Revision, RosterAgent, SessionId, SessionStatus, StoredRule, UsageRow, Workspace, WorkspaceId,
    WorkspaceState,
};
