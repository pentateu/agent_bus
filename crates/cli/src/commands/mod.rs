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

use anyhow::{Result, bail};

use agent_bus_protocol::Response;

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
