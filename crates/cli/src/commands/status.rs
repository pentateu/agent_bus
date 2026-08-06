//! `agent-bus status` — report daemon state.

use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{cli::ExitCode, client::Client, commands::unexpected, output};

/// Report daemon state.
///
/// # Errors
/// Returns an error if the daemon cannot be reached or fails the request.
pub fn run(json: bool) -> Result<ExitCode> {
    let mut client = Client::connect()?;
    match client.request(&Request::Status)? {
        Response::Status { status } => {
            output::print_status(&status, json)?;
            Ok(ExitCode::Success)
        }
        other => Err(unexpected(&other)),
    }
}
