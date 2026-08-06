//! Pure domain logic for agent-bus: topics, messages, cursors, retention.
//!
//! This crate performs no I/O and contains no async code. Everything here is
//! a pure function or plain data, which keeps the interesting logic testable
//! without sockets or temp directories.
