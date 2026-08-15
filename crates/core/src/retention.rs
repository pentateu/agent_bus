/// How long the daemon stays alive with no activity before exiting: 1.5 hours.
///
/// Deliberately longer than [`RetentionPolicy::default`], so that by the time
/// the daemon exits its logs have already been pruned to nothing and no
/// message of value is lost to the shutdown.
pub const IDLE_SHUTDOWN_SECS: u64 = 5400;

/// Default `wait` timeout when the client does not supply one: 30 minutes.
///
/// Bounded so a client never blocks forever against a harness tool timeout.
pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 1800;

/// Hard ceiling on a client-supplied `wait` timeout: 48 hours.
///
/// A sane bound is still needed — `u64::MAX` seconds overflowed
/// `Instant + Duration` and panicked the connection task once — but it is no
/// longer tied to [`IDLE_SHUTDOWN_SECS`]: the daemon stays alive while clients
/// are parked in a wait, so a wait can legitimately outlive the idle window.
/// 48h is a safety rail, not a contract: retention still empties the log within
/// an hour, so a long wait mostly just times out.
pub const MAX_WAIT_TIMEOUT_SECS: u64 = 48 * 60 * 60;

/// Age-based retention for a partition log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Messages strictly younger than this are kept.
    pub max_age_secs: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self { max_age_secs: 3600 }
    }
}

impl RetentionPolicy {
    /// Is a message of this age still retained?
    #[must_use]
    pub fn retains(&self, age_secs: u64) -> bool {
        age_secs < self.max_age_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_window_is_one_hour() {
        assert_eq!(RetentionPolicy::default().max_age_secs, 3600);
    }

    #[test]
    fn partitions_messages_into_kept_and_pruned() {
        let policy = RetentionPolicy::default();
        // ages in seconds relative to "now"
        let ages = [0_u64, 100, 3599, 3600, 7200];
        let kept: Vec<bool> = ages.iter().map(|a| policy.retains(*a)).collect();
        assert_eq!(kept, vec![true, true, true, false, false]);
    }

    #[test]
    fn custom_window_is_respected() {
        let policy = RetentionPolicy { max_age_secs: 60 };
        assert!(policy.retains(59));
        assert!(!policy.retains(60));
    }

    #[test]
    fn idle_shutdown_is_above_retention_window() {
        // The daemon must outlive its data, so that when it exits the logs are
        // already empty and nothing of value is lost.
        assert!(IDLE_SHUTDOWN_SECS > RetentionPolicy::default().max_age_secs);
        assert_eq!(IDLE_SHUTDOWN_SECS, 5400);
    }

    #[test]
    fn wait_timeouts_are_sane_and_ordered() {
        // Constant comparisons are checked at compile time; the const block
        // also satisfies clippy's assertion-on-constants lint.
        const _: () = assert!(DEFAULT_WAIT_TIMEOUT_SECS < MAX_WAIT_TIMEOUT_SECS);
        const _: () = assert!(MAX_WAIT_TIMEOUT_SECS > IDLE_SHUTDOWN_SECS);
        // The ceiling exists to bound the wire value; it must be a plausible
        // cap rather than a rename of the default.
        assert_eq!(MAX_WAIT_TIMEOUT_SECS, 48 * 60 * 60);
    }
}
