//! `agent-bus wait` — block for one unread message.

use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{cli::ExitCode, client::Client, commands::unexpected, output};

/// Block for one unread message.
///
/// Delivery is exclusive: the daemon marks the returned message delivered for
/// the whole pattern before answering, so no other consumer receives it and a
/// later `wait`/`read` under the same pattern will not return it again.
///
/// Exits 2 on timeout so `while agent-bus wait ...; do ...; done` terminates
/// cleanly rather than looping on an error.
///
/// # Errors
/// Returns an error if the pattern is invalid or the daemon fails the request.
pub fn run(
    pattern: &str,
    label: String,
    timeout_secs: Option<u64>,
    json: bool,
) -> Result<ExitCode> {
    let mut client = Client::connect()?;
    let response =
        client.request(&Request::Wait { pattern: pattern.to_owned(), label, timeout_secs })?;

    match response {
        Response::Messages { messages } => {
            output::print_messages(&messages, json)?;
            Ok(ExitCode::Success)
        }
        Response::Timeout => {
            if json {
                println!("{}", serde_json::json!({ "timeout": true }));
            } else {
                eprintln!("timed out with no messages");
            }
            Ok(ExitCode::Timeout)
        }
        other => Err(unexpected(&other)),
    }
}
