//! Pure domain logic for agent-bus: topics, messages, cursors, retention,
//! and the on-disk path layout.
//!
//! This crate touches neither the filesystem nor the network, and contains no
//! async code. Almost everything here is a pure function or plain data, which
//! keeps the interesting logic testable without sockets or temp directories.
//! The two exceptions read ambient process state but write nothing:
//! [`message`] reads the system clock, and [`paths`] reads environment
//! variables to locate the state directory — it only *computes* those paths;
//! opening them is the caller's job.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod cursor;
pub mod error;
pub mod message;
pub mod paths;
pub mod retention;
pub mod topic;

pub use cursor::CursorStore;
pub use error::CoreError;
pub use message::{Message, Priority};
pub use retention::{IDLE_SHUTDOWN_SECS, RetentionPolicy};
pub use topic::{Pattern, Topic};
