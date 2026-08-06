//! All daemon state, owned by one task and mutated through `&mut`.
//!
//! Partitions are opened lazily: the daemon has no registry of known projects,
//! so the first request naming a partition is what brings it into existence.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    path::PathBuf,
    time::Instant,
};

use agent_bus_core::RetentionPolicy;
use anyhow::Result;

use crate::partition::Partition;

/// All daemon state: one entry per partition, created on demand.
pub struct BusState {
    state_dir: PathBuf,
    partitions: BTreeMap<String, Partition>,
    policy: RetentionPolicy,
    started: Instant,
    /// Last time any client did anything, for idle shutdown.
    last_activity: Instant,
}

impl BusState {
    #[must_use]
    pub fn new(state_dir: PathBuf) -> Self {
        let now = Instant::now();
        Self {
            state_dir,
            partitions: BTreeMap::new(),
            policy: RetentionPolicy::default(),
            started: now,
            last_activity: now,
        }
    }

    #[must_use]
    pub fn partition_exists(&self, name: &str) -> bool {
        self.partitions.contains_key(name)
    }

    /// Get a partition, opening it from disk the first time it is touched.
    ///
    /// Written as a single `entry` match so the "just inserted, so the lookup
    /// cannot fail" case is impossible by construction rather than by comment:
    /// there is no second lookup and therefore no panic path in a process that
    /// must not crash.
    ///
    /// # Errors
    /// Returns an error if the partition's files cannot be opened.
    pub fn partition_mut(&mut self, name: &str) -> Result<&mut Partition> {
        match self.partitions.entry(name.to_owned()) {
            Entry::Occupied(existing) => Ok(existing.into_mut()),
            Entry::Vacant(slot) => {
                let partition = Partition::open(&self.state_dir, name)?;
                Ok(slot.insert(partition))
            }
        }
    }

    /// Names of the currently-open partitions, for assertions.
    #[cfg(test)]
    pub fn partition_names(&self) -> Vec<String> {
        self.partitions.keys().cloned().collect()
    }

    pub fn partitions(&self) -> impl Iterator<Item = &Partition> {
        self.partitions.values()
    }

    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    #[must_use]
    pub fn idle_secs(&self) -> u64 {
        self.last_activity.elapsed().as_secs()
    }

    #[must_use]
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Prune every partition. Returns the total number of messages removed.
    ///
    /// # Errors
    /// Returns an error if any partition's rewrite fails.
    pub fn prune_all(&mut self, now: u64) -> Result<usize> {
        let policy = self.policy;
        let mut removed = 0;
        for partition in self.partitions.values_mut() {
            removed += partition.prune(&policy, now)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn partitions_are_created_on_demand_and_reused() {
        let dir = TempDir::new().unwrap();
        let mut state = BusState::new(dir.path().to_path_buf());

        assert!(!state.partition_exists("iot_base"));
        state.partition_mut("iot_base").unwrap();
        assert!(state.partition_exists("iot_base"));

        // Second call must reuse, not recreate.
        state.partition_mut("iot_base").unwrap();
        assert_eq!(state.partition_names(), vec!["iot_base".to_owned()]);
    }

    #[test]
    fn partitions_are_isolated_from_each_other() {
        use agent_bus_core::{Message, Pattern, Priority, Topic};

        let dir = TempDir::new().unwrap();
        let mut state = BusState::new(dir.path().to_path_buf());

        let m = Message::new(
            Topic::parse("iot_base/dev_01").unwrap(),
            "secret".to_owned(),
            Priority::Normal,
            None,
        );
        state.partition_mut("iot_base").unwrap().publish(m).unwrap();

        let other = state.partition_mut("other_project").unwrap();
        let pattern = Pattern::parse("other_project/**").unwrap();
        assert!(
            other.unread(&pattern, "spy").is_empty(),
            "a message in iot_base must be invisible to other_project"
        );
    }
}
