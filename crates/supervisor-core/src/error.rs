//! Errors produced by pure supervisor domain logic.
//!
//! The core crate is pure: everything here is either plain data or a pure
//! function, so the error type covers validation and contract violations
//! rather than I/O failures (those belong to the daemon, which uses `anyhow`).

use thiserror::Error;

/// Errors produced by `supervisor-core` pure logic.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid rule {id:?}: {reason}")]
    InvalidRule { id: String, reason: String },

    #[error("invalid workflow {name:?}: {reason}")]
    InvalidWorkflow { name: String, reason: String },

    #[error("invalid graph {id:?}: {reason}")]
    InvalidGraph { id: String, reason: String },

    #[error("invalid journal record: {0}")]
    MalformedRecord(String),

    #[error("invalid config: {0}")]
    InvalidConfig(String),

    #[error("invalid port {port}: {reason}")]
    InvalidPort { port: u16, reason: &'static str },

    #[error("invalid ack: {0}")]
    InvalidAck(String),

    #[error("invalid signature: {0}")]
    InvalidSignature(String),
}

pub type CoreResult<T> = Result<T, CoreError>;
