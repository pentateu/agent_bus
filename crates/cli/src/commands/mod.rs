//! One module per subcommand. Each returns the process exit code.

pub mod daemon;
pub mod follow;
pub mod history;
pub mod hook;
pub mod post;
pub mod read;
pub mod status;
pub mod stop;
pub mod wait;

use agent_bus_core::{Message, Pattern};
use agent_bus_protocol::{Request, Response};
use anyhow::{Result, bail};

use crate::{client::Client, output};

/// Print a batch, then acknowledge it so the cursor advances.
///
/// The ack is deliberately sent *after* the messages are on stdout: `wait` and
/// `read` are at-least-once, so a client killed mid-print re-reads the batch
/// next time instead of losing it. Shared by `wait` and `read`, which differ
/// only in the request that produced the batch.
///
/// `pattern` is sent alongside the partition because the daemon keys cursors on
/// (subscriber, pattern). Acking without it would advance a single per-
/// subscriber position past messages this pattern never selected, destroying
/// them for every other pattern the same subscriber reads.
///
/// # Errors
/// Returns an error if rendering fails or the daemon rejects the ack.
pub fn print_and_ack(
    client: &mut Client,
    messages: &[Message],
    pattern: &Pattern,
    subscriber: String,
    json: bool,
) -> Result<()> {
    output::print_messages(messages, json)?;

    if let Some(last) = messages.last() {
        let ack = client.request(&Request::Ack {
            partition: pattern.partition().to_owned(),
            pattern: pattern.as_str().to_owned(),
            subscriber,
            id: last.id.to_string(),
        })?;
        if let Response::Error { message } = ack {
            bail!("{message}");
        }
    }
    Ok(())
}

/// Turn an unexpected response into an error, surfacing daemon-reported
/// failures rather than ignoring them.
///
/// Returns the error directly rather than a `Result` because every caller is
/// already in a `match` arm that has nothing to return on success.
#[must_use]
pub fn unexpected(response: &Response) -> anyhow::Error {
    match response {
        Response::Error { message } => anyhow::anyhow!("{message}"),
        other => anyhow::anyhow!("unexpected daemon response: {other:?}"),
    }
}

/// Read a message body from stdin when it was not given as an argument.
///
/// # Errors
/// Returns an error if stdin cannot be read or is empty.
pub fn body_from_stdin() -> Result<String> {
    use std::io::Read;
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    let trimmed = buffer.trim_end_matches('\n').to_owned();
    if trimmed.is_empty() {
        bail!("no message body: pass it as an argument or pipe it on stdin");
    }
    Ok(trimmed)
}
