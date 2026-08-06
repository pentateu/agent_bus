/// How long the daemon stays alive with no activity before exiting: 1.5 hours.
///
/// Deliberately longer than [`RetentionPolicy::default`], so that by the time
/// the daemon exits its logs have already been pruned to nothing and no
/// message of value is lost to the shutdown.
pub const IDLE_SHUTDOWN_SECS: u64 = 5400;

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
}
