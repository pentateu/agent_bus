//! `agent-bus read` — drain everything unread, without blocking.

use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{cli::ExitCode, client::Client, commands::unexpected, output};

/// Drain everything unread, without blocking.
///
/// Delivery is exclusive: everything returned is marked delivered for the whole
/// pattern, so a second `read` (or any other consumer) under the same pattern
/// will not see it again.
///
/// # Errors
/// Returns an error if the pattern is invalid or the daemon fails the request.
pub fn run(pattern: &str, label: String, json: bool) -> Result<ExitCode> {
    let mut client = Client::connect()?;
    let response = client.request(&Request::Read { pattern: pattern.to_owned(), label })?;

    match response {
        Response::Messages { messages } => {
            output::print_messages(&messages, json)?;
            Ok(ExitCode::Success)
        }
        other => Err(unexpected(&other)),
    }
}
