//! The long-lived supervisor process: all async services + clients.
//!
//! This crate owns every socket, process, and file: the `SQLite` projection, the
//! append-only journal, the internal event bus, the fleet state, the opencode /
//! cmux / SSE / manager clients, the workspace manager, delivery, the decision
//! layer, the loopback API, and the dashboard. Pure domain logic lives in
//! `supervisor-core`.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent_assets;
pub mod api;
pub mod bus;
pub mod clients;
pub mod db;
pub mod journal;
pub mod launchd;
pub mod secrets;
pub mod services;
pub mod state;
