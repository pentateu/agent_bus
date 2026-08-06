//! `agent-bus wait` — block for one unread message.

use agent_bus_core::Pattern;
use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{
    cli::ExitCode,
    client::Client,
    commands::{print_and_ack, unexpected},
};

/// Block for one unread message.
///
/// Exits 2 on timeout so `while agent-bus wait ...; do ...; done` terminates
/// cleanly rather than looping on an error.
///
/// # Errors
/// Returns an error if the pattern is invalid or the daemon fails the request.
pub fn run(
    pattern: &str,
    subscriber: String,
    timeout_secs: Option<u64>,
    json: bool,
) -> Result<ExitCode> {
    // Parsed client-side purely to learn the partition for the ack. The daemon
    // parses it again authoritatively.
    let parsed = Pattern::parse(pattern)?;

    let mut client = Client::connect()?;
    let response = client.request(&Request::Wait {
        pattern: pattern.to_owned(),
        subscriber: subscriber.clone(),
        timeout_secs,
    })?;

    match response {
        Response::Messages { messages } => {
            print_and_ack(&mut client, &messages, parsed.partition(), subscriber, json)?;
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
