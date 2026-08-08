//! The agent-bus daemon: one per OS user, owns all partition state.
//!
//! Started automatically by the CLI. Exits when idle or on `agent-bus stop`.

#![cfg_attr(test, allow(clippy::unwrap_used))]

mod handler;
mod log;
mod metrics;
mod partition;
mod server;
mod state;
mod sweep;

use std::sync::Arc;

use agent_bus_core::paths::{lock_path, socket_path, state_dir_from_env};
use anyhow::{Context, Result};
use fs2::FileExt;
use tokio::sync::Mutex;

use crate::state::BusState;

fn main() -> Result<()> {
    let state_dir = state_dir_from_env();
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating state directory {}", state_dir.display()))?;

    // Exactly one daemon per state directory. Several agents across several
    // IDEs will race to auto-start; the losers exit quietly and connect to the
    // winner's socket.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path(&state_dir))
        .with_context(|| format!("opening lock file in {}", state_dir.display()))?;

    if lock_file.try_lock_exclusive().is_err() {
        // Another daemon owns this state directory. Not an error.
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let socket = socket_path(&state_dir);
    let state = Arc::new(Mutex::new(BusState::new(state_dir)));

    let outcome = runtime.block_on(server::serve(&socket, state));

    // Unlocked on both paths, not just success: the OS would release it at exit
    // anyway, but an explicit call that only ran when things went well would be
    // actively misleading about where the lock's lifetime ends.
    let _ = FileExt::unlock(&lock_file);
    outcome
}
