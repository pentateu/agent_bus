//! `agent-bus follow` — stream messages until interrupted.

use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{cli::ExitCode, client::Client, commands::unexpected, output};

/// Stream messages until the daemon closes or the process is interrupted.
///
/// The daemon advances the cursor itself as it writes each batch, so no
/// client-side ack is sent here; one would be pure redundancy.
///
/// # Errors
/// Returns an error if the daemon reports a failure or the stream breaks.
pub fn run(pattern: String, subscriber: String, json: bool) -> Result<ExitCode> {
    let mut client = Client::connect()?;
    let first = client.request(&Request::Follow { pattern, subscriber })?;

    let mut current = Some(first);
    while let Some(response) = current {
        match response {
            Response::Messages { messages } => output::print_messages(&messages, json)?,
            Response::Error { message } => anyhow::bail!("{message}"),
            other => return Err(unexpected(&other)),
        }
        current = client.read_optional()?;
    }
    Ok(ExitCode::Success)
}
