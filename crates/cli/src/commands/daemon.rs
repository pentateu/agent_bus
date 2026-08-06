//! `agent-bus daemon` — ensure the daemon and a partition exist.

use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{cli::ExitCode, client::Client, commands::unexpected};

/// Ensure the daemon and partition exist.
///
/// Idempotent: running it against an already-live partition just acknowledges.
///
/// # Errors
/// Returns an error if the daemon cannot be started or rejects the pattern.
pub fn run(pattern: String, json: bool) -> Result<ExitCode> {
    let mut client = Client::connect()?;
    match client.request(&Request::Ensure { pattern })? {
        Response::Ensured { partition, already_running } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "partition": partition,
                        "already_running": already_running
                    })
                );
            } else if already_running {
                println!("partition {partition} already running");
            } else {
                println!("partition {partition} ready");
            }
            Ok(ExitCode::Success)
        }
        other => Err(unexpected(&other)),
    }
}
