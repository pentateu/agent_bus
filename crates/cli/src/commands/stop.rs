//! `agent-bus stop` — shut the daemon down.

use std::os::unix::net::UnixStream;

use agent_bus_protocol::{Request, Response};
use anyhow::Result;

use crate::{cli::ExitCode, client::Client, commands::unexpected};

/// Shut the daemon down.
///
/// # Errors
/// Returns an error if a live daemon rejects the request.
pub fn run(json: bool) -> Result<ExitCode> {
    // Never auto-start just to stop. `Client::connect` would spawn a daemon if
    // the socket were missing or stale, which for `stop` is exactly backwards:
    // a crashed daemon leaves its socket file behind, so an `exists()` check
    // alone is not enough. Probing with a real connect distinguishes "listening"
    // from "stale inode" and only then hands the live stream to the client.
    let Ok(stream) = UnixStream::connect(crate::client::socket_path()) else {
        if json {
            println!("{}", serde_json::json!({ "stopped": false, "reason": "not running" }));
        } else {
            println!("daemon is not running");
        }
        return Ok(ExitCode::Success);
    };

    let mut client = Client::from_connected(stream)?;
    match client.request(&Request::Stop)? {
        Response::Ok => {
            if json {
                println!("{}", serde_json::json!({ "stopped": true }));
            } else {
                println!("daemon stopped");
            }
            Ok(ExitCode::Success)
        }
        other => Err(unexpected(&other)),
    }
}
