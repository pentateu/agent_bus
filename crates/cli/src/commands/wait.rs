//! `agent-bus wait` — block for one unread message.

use std::time::{Duration, Instant};

use agent_bus_core::{DEFAULT_WAIT_TIMEOUT_SECS, MAX_WAIT_TIMEOUT_SECS};
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
/// A daemon restart mid-wait used to close the connection and fail the whole
/// wait with "daemon closed the connection without responding". The wait now
/// reconnects (auto-starting a fresh daemon) and resumes for the remaining
/// time, bounded by the same ceiling the daemon applies, so a resident agent
/// survives the daemon churn that put it here.
///
/// # Errors
/// Returns an error if the pattern is invalid or the daemon fails the request.
pub fn run(pattern: &str, label: &str, timeout_secs: Option<u64>, json: bool) -> Result<ExitCode> {
    // Clamped on the client so the retry loop and the daemon agree on how long
    // a wait may last. Re-submitting with a larger deadline after a reconnect
    // would let a crash-loop stretch a "4h" wait indefinitely.
    let budget = timeout_secs.unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS).min(MAX_WAIT_TIMEOUT_SECS);
    let deadline = Instant::now() + Duration::from_secs(budget);
    let pattern = pattern.to_owned();
    let label = label.to_owned();
    let mut last_error: Option<anyhow::Error> = None;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() && last_error.is_some() {
            // Tried and the daemon never answered within the budget: surface
            // whatever was most recently wrong (exit 3 for unavailability).
            // A fresh `--timeout 0` has no error yet and must still ask the
            // daemon once (I-17: it returns Timeout → exit 2, matching the
            // documented "nothing pending" contract).
            return Err(last_error.unwrap());
        }

        let response = match Client::connect().and_then(|mut client| {
            client.request(&Request::Wait {
                pattern: pattern.clone(),
                label: label.clone(),
                timeout_secs: Some(remaining.as_secs()),
            })
        }) {
            Ok(response) => response,
            Err(e) => {
                last_error = Some(e);
                eprintln!(
                    "agent-bus: daemon unreachable mid-wait; retrying ({}s left)",
                    remaining.as_secs()
                );
                // Give a restarting daemon a moment before connecting again.
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        return match response {
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
        };
    }
}
