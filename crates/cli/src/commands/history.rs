//! `agent-bus history` — replay past messages, ignoring cursors.

use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{cli::ExitCode, client::Client, commands::unexpected, output};

/// Replay past messages, ignoring cursors.
///
/// `since_secs: None` means the full retained window, so bare `history` never
/// silently truncates.
///
/// # Errors
/// Returns an error if the daemon fails the request.
pub fn run(pattern: String, since_secs: Option<u64>, json: bool) -> Result<ExitCode> {
    let mut client = Client::connect()?;
    match client.request(&Request::History { pattern, since_secs })? {
        Response::Messages { messages } => {
            output::print_messages(&messages, json)?;
            Ok(ExitCode::Success)
        }
        other => Err(unexpected(&other)),
    }
}
