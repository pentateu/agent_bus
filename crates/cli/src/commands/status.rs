//! `agent-bus status` — report daemon state.

use agent_bus_core::paths as core_paths;
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
            if json {
                output::print_status(&status, true)?;
            } else {
                output::print_status(&status, false)?;
                // Where the daemon writes its diagnostics, so a crash is
                // something a human can actually go read.
                println!("  log:    {}", core_paths::daemon_log_path().display());
            }
            Ok(ExitCode::Success)
        }
        other => Err(unexpected(&other)),
    }
}
