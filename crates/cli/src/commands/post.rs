//! `agent-bus post` — publish a message to a topic.

use agent_bus_protocol::{Priority, Request, Response};
use anyhow::Result;

use crate::{
    cli::ExitCode,
    client::Client,
    commands::{body_from_stdin, unexpected},
};

/// Publish a message.
///
/// # Errors
/// Returns an error if the body cannot be read or the daemon rejects the post.
pub fn run(
    topic: String,
    message: Option<String>,
    priority: &str,
    from: Option<String>,
    json: bool,
) -> Result<ExitCode> {
    let body = match message {
        // Rejected here as well as in `body_from_stdin` so the two ways of
        // supplying a body behave the same: an empty message carries nothing
        // and is a mistake either way, not a valid post.
        Some(body) if body.is_empty() => {
            anyhow::bail!("message body must not be empty");
        }
        Some(body) => body,
        None => body_from_stdin()?,
    };
    // `--priority` is constrained to these two by clap's value parser, so the
    // fallback here is unreachable in practice.
    let priority = if priority == "high" { Priority::High } else { Priority::Normal };

    let mut client = Client::connect()?;
    let response = client.request(&Request::Post { topic, body, priority, from })?;

    match response {
        Response::Posted { id } => {
            if json {
                println!("{}", serde_json::json!({ "id": id }));
            } else {
                println!("posted {id}");
            }
            Ok(ExitCode::Success)
        }
        other => Err(unexpected(&other)),
    }
}
