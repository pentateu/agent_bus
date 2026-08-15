use thiserror::Error;

/// Errors produced by pure domain logic.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid topic {input:?}: {reason}")]
    InvalidTopic { input: String, reason: &'static str },

    #[error("invalid pattern {input:?}: {reason}")]
    InvalidPattern { input: String, reason: &'static str },

    #[error("invalid partition {input:?}: {reason}")]
    InvalidPartition { input: String, reason: &'static str },

    #[error("malformed message record: {0}")]
    MalformedRecord(String),
}
