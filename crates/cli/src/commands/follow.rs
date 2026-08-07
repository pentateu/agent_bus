//! `agent-bus follow` — stream messages until interrupted.

use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{cli::ExitCode, client::Client, commands::unexpected, output};

/// Stream messages until the daemon closes or the process is interrupted.
///
/// The daemon advances the pattern's position as it writes each batch, so a
/// streamed message is never delivered again — not to this client, and not to
/// any other consumer of the same pattern.
///
/// A `follow` only ends normally when the user interrupts it — the daemon
/// streams indefinitely otherwise. So reaching end of stream means the daemon
/// went away, and that is reported as a failure rather than success: returning
/// 0 here once made a broken stream indistinguishable from a clean exit, and
/// the daemon-side error was going to /dev/null.
///
/// # Errors
/// Returns an error if the daemon reports a failure or the stream ends
/// unexpectedly.
pub fn run(pattern: String, label: String, json: bool) -> Result<ExitCode> {
    let mut client = Client::connect()?;
    let first = client.request(&Request::Follow { pattern, label })?;

    let mut current = Some(first);
    while let Some(response) = current {
        match response {
            Response::Messages { messages } => output::print_messages(&messages, json)?,
            Response::Error { message } => anyhow::bail!("{message}"),
            other => return Err(unexpected(&other)),
        }
        current = client.read_optional()?;
    }

    anyhow::bail!("the daemon closed the follow stream unexpectedly")
}
