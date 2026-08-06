//! Background maintenance: retention pruning and idle shutdown.

use std::sync::Arc;

use agent_bus_core::IDLE_SHUTDOWN_SECS;
use tokio::sync::{Mutex, mpsc};

use crate::{handler::now_secs, state::BusState};

/// How often to prune and check for idleness.
const SWEEP_INTERVAL_SECS: u64 = 60;

/// Prune expired messages every minute and signal shutdown once the daemon has
/// been idle past [`IDLE_SHUTDOWN_SECS`].
///
/// The state lock is held across `prune_all`, which rewrites log files
/// synchronously. At the volumes this bus is built for — a handful of agents
/// exchanging short messages, pruned hourly — that rewrite is a few milliseconds
/// on a file that is already in page cache, so the simplicity of holding the
/// lock beats the bookkeeping of moving the work to a blocking pool.
pub async fn run(state: Arc<Mutex<BusState>>, shutdown: mpsc::Sender<()>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
    // The first tick fires immediately; skip it so we do not prune at startup.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        let mut guard = state.lock().await;

        if let Err(e) = guard.prune_all(now_secs()) {
            eprintln!("agent-bus: retention sweep failed: {e:#}");
        }

        let idle = guard.idle_secs();
        // Released before signalling: `serve` drops the state alongside the
        // listener on its way out, and a held guard would stall that.
        drop(guard);

        if idle >= IDLE_SHUTDOWN_SECS {
            let _ = shutdown.send(()).await;
            return;
        }
    }
}
