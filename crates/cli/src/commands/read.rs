//! `agent-bus read` — drain everything unread, without blocking.

use agent_bus_core::Pattern;
use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{
    cli::ExitCode,
    client::Client,
    commands::{print_and_ack, unexpected},
};

/// Drain everything unread, without blocking.
///
/// # Errors
/// Returns an error if the pattern is invalid or the daemon fails the request.
pub fn run(pattern: &str, subscriber: String, json: bool) -> Result<ExitCode> {
    let parsed = Pattern::parse(pattern)?;

    let mut client = Client::connect()?;
    let response = client
        .request(&Request::Read { pattern: pattern.to_owned(), subscriber: subscriber.clone() })?;

    match response {
        Response::Messages { messages } => {
            print_and_ack(&mut client, &messages, &parsed, subscriber, json)?;
            Ok(ExitCode::Success)
        }
        other => Err(unexpected(&other)),
    }
}
