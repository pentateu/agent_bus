//! Background maintenance: retention pruning and idle shutdown.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use agent_bus_core::IDLE_SHUTDOWN_SECS;
use tokio::sync::{Mutex, mpsc};

use crate::{handler::now_secs, logging::log_msg, state::BusState};

/// How often to prune and check for idleness.
const SWEEP_INTERVAL_SECS: u64 = 60;

/// The sweep interval: `AGENT_BUS_SWEEP_INTERVAL_SECS` override, else the
/// constant (test hook — lets an integration test drive shutdown quickly).
fn sweep_interval_secs() -> u64 {
    std::env::var("AGENT_BUS_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SWEEP_INTERVAL_SECS)
}

/// The idle shutdown threshold: `AGENT_BUS_IDLE_SHUTDOWN_SECS` override, else
/// [`IDLE_SHUTDOWN_SECS`] (test hook — I-16 integration coverage).
fn idle_shutdown_secs() -> u64 {
    std::env::var("AGENT_BUS_IDLE_SHUTDOWN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(IDLE_SHUTDOWN_SECS)
}

/// Prune expired messages every minute and signal shutdown once the daemon has
/// been idle past the idle threshold.
///
/// The state lock is held across `prune_all`, which rewrites log files
/// synchronously. At the volumes this bus is built for — a handful of agents
/// exchanging short messages, pruned hourly — that rewrite is a few milliseconds
/// on a file that is already in page cache, so the simplicity of holding the
/// lock beats the bookkeeping of moving the work to a blocking pool.
pub async fn run(
    state: Arc<Mutex<BusState>>,
    shutdown: mpsc::Sender<()>,
    active_waiters: Arc<AtomicU64>,
    active_followers: Arc<AtomicU64>,
) {
    let sweep_interval = sweep_interval_secs();
    let threshold = idle_shutdown_secs();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(sweep_interval));
    // The first tick fires immediately; skip it so we do not prune at startup.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        let mut guard = state.lock().await;

        if let Err(e) = guard.prune_all(now_secs()) {
            log_msg(&format!("retention sweep failed: {e:#}"));
        }

        let idle = guard.idle_secs();
        // Released before signalling: `serve` drops the state alongside the
        // listener on its way out, and a held guard would stall that.
        drop(guard);

        let parked =
            active_waiters.load(Ordering::Relaxed) + active_followers.load(Ordering::Relaxed);
        if should_shutdown(idle, parked, threshold) {
            log_msg(&format!("idle for {idle}s with no parked clients; shutting down"));
            let _ = shutdown.send(()).await;
            return;
        }
    }
}

/// Should the daemon exit for idleness?
///
/// Parked `wait`/`follow` clients keep the daemon alive: they are active use of
/// the bus even though they issue no requests, and shutting down under them
/// drops their connections with no explanation — the "daemon closed the
/// connection without responding" failures agents reported on long waits.
#[must_use]
fn should_shutdown(idle_secs: u64, parked_clients: u64, threshold_secs: u64) -> bool {
    parked_clients == 0 && idle_secs >= threshold_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_daemon_with_no_parked_clients_shuts_down() {
        assert!(should_shutdown(IDLE_SHUTDOWN_SECS, 0, IDLE_SHUTDOWN_SECS));
        assert!(should_shutdown(IDLE_SHUTDOWN_SECS + 1, 0, IDLE_SHUTDOWN_SECS));
    }

    #[test]
    fn a_parked_client_keeps_the_daemon_alive() {
        assert!(
            !should_shutdown(IDLE_SHUTDOWN_SECS, 1, IDLE_SHUTDOWN_SECS),
            "a waiting client is active use, not idleness"
        );
        assert!(!should_shutdown(u64::MAX, 1, IDLE_SHUTDOWN_SECS));
    }

    #[test]
    fn a_busy_daemon_does_not_shut_down() {
        assert!(!should_shutdown(0, 0, IDLE_SHUTDOWN_SECS));
        assert!(!should_shutdown(IDLE_SHUTDOWN_SECS - 1, 0, IDLE_SHUTDOWN_SECS));
    }

    #[test]
    fn the_policy_is_the_default_retention_window() {
        assert_eq!(IDLE_SHUTDOWN_SECS, 5400);
    }
}
