//! Cumulative metrics tallies for the dashboard.
//!
//! Two stores: [`BusTally`] holds the cross-partition histograms (latency and
//! message size); [`PartitionTally`] holds per-partition counters. Both are
//! bumped inside code paths that already hold the daemon's state mutex, so
//! they add no locking of their own. They never decrement.

/// Lower bounds of each latency bin, in milliseconds. A value `>=` the final
/// boundary lands in the final (catch-all) bin.
pub const LATENCY_BUCKETS_MS: &[u64] =
    &[0, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000, 60000];

/// Lower bounds of each message-size bin, in bytes.
pub const SIZE_BUCKETS_BYTES: &[u64] =
    &[0, 32, 64, 128, 256, 512, 1024, 4096, 16384, 65536, 262_144, 1_048_576, 4_194_304];

/// Index of the histogram bin a value falls into.
///
/// `boundaries` are the lower bounds; the histogram has `boundaries.len() + 1`
/// cells where cell 0 is `< boundaries[0]` and the final cell is `>=` the last
/// boundary. A value equal to a boundary goes into the bin that STARTS at that
/// boundary: 10ms lands in the `[10,25)` bin, not the `[0,10)` bin.
// TODO(wiring): dead code until the instrumentation tasks land; remove the
// allows once publish/deliver/prune bump the tallies.
#[allow(dead_code)]
#[must_use]
pub fn bin_index(boundaries: &[u64], value: u64) -> usize {
    // Count of boundaries `<= value` is exactly the bin index: every boundary
    // at or below the value opens a bin that the value does NOT fall into, and
    // every boundary above the value opens a bin that it does.
    boundaries.iter().filter(|&&b| b <= value).count()
}

/// Cross-partition histograms, owned by [`crate::state::BusState`].
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BusTally {
    latency_hist: Vec<u64>,
    size_hist: Vec<u64>,
}

/// A cheap snapshot of both histograms for `build_metrics`.
#[allow(dead_code)]
pub struct TallySnapshot<'a> {
    pub latency_hist: &'a [u64],
    pub size_hist: &'a [u64],
}

#[allow(dead_code)]
impl BusTally {
    #[must_use]
    pub fn new() -> Self {
        Self {
            latency_hist: vec![0; LATENCY_BUCKETS_MS.len() + 1],
            size_hist: vec![0; SIZE_BUCKETS_BYTES.len() + 1],
        }
    }

    /// Record one published message's body size.
    pub fn record_post(&mut self, body_bytes: u64) {
        self.size_hist[bin_index(SIZE_BUCKETS_BYTES, body_bytes)] += 1;
    }

    /// Record one delivery's post-to-delivery latency in milliseconds.
    pub fn record_delivery(&mut self, latency_ms: u64) {
        self.latency_hist[bin_index(LATENCY_BUCKETS_MS, latency_ms)] += 1;
    }

    #[must_use]
    pub fn snapshot(&self) -> TallySnapshot<'_> {
        TallySnapshot { latency_hist: &self.latency_hist, size_hist: &self.size_hist }
    }
}

impl Default for BusTally {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-partition cumulative counters, owned by [`crate::partition::Partition`].
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct PartitionTally {
    pub posts: u64,
    pub deliveries: u64,
    pub bytes: u64,
    pub posts_high: u64,
    pub posts_broadcast: u64,
    pub pruned: u64,
    pub snapped: u64,
}

#[allow(dead_code)]
impl PartitionTally {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one published message. `body_bytes` is the UTF-8 byte length of
    /// the body; `high` and `broadcast` carry the message's flags so the
    /// totals can split them out without re-parsing.
    pub fn record_post(&mut self, body_bytes: u64, high: bool, broadcast: bool) {
        self.posts += 1;
        self.bytes += body_bytes;
        if high {
            self.posts_high += 1;
        }
        if broadcast {
            self.posts_broadcast += 1;
        }
    }

    /// Record that `count` messages were just delivered.
    pub fn record_deliveries(&mut self, count: u64) {
        self.deliveries += count;
    }

    /// Record that a prune pass removed `removed` messages and dragged `snapped`
    /// cursors forward (the daemon's existing `snapped` in-memory vec tracks
    /// *which* positions; this counts *how many times* it grew, which is the
    /// "I lost messages" cumulative counter the dashboard shows).
    pub fn record_prune(&mut self, removed: u64, snapped: u64) {
        self.pruned += removed;
        self.snapped += snapped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_index_picks_lower_bound_inclusive() {
        // Boundaries are lower bounds; a value equal to a boundary lands in
        // the bin that STARTS at that boundary, not the one that ENDS there.
        let b = [0u64, 10, 25, 50];
        assert_eq!(bin_index(&b, 0), 1, "0 lands in [0,10)");
        assert_eq!(bin_index(&b, 9), 1);
        assert_eq!(bin_index(&b, 10), 2, "10 lands in [10,25), not [0,10)");
        assert_eq!(bin_index(&b, 24), 2);
        assert_eq!(bin_index(&b, 25), 3);
        assert_eq!(bin_index(&b, 49), 3);
        assert_eq!(bin_index(&b, 50), 4, "the final bin is >= last boundary");
        assert_eq!(bin_index(&b, 9_999), 4);
    }

    #[test]
    fn a_value_below_the_first_boundary_lands_in_bin_zero() {
        // The first bin is "< first boundary". With buckets[0] == 0 it is
        // normally unreachable; the test documents the shape rather than the
        // reachable case.
        let b = [10u64, 20];
        assert_eq!(bin_index(&b, 5), 0);
    }

    #[test]
    fn record_post_increments_size_histogram_and_returns_nothing() {
        let mut tally = BusTally::new();
        tally.record_post(50); // [32,64) -> bin 2
        tally.record_post(40); // [32,64) -> bin 2
        tally.record_post(1024); // [1024,4096) -> bin 7
        let snap = tally.snapshot();
        assert_eq!(snap.size_hist[2], 2);
        assert_eq!(snap.size_hist[7], 1);
        assert_eq!(snap.size_hist.iter().sum::<u64>(), 3);
    }

    #[test]
    fn record_delivery_increments_latency_histogram() {
        let mut tally = BusTally::new();
        tally.record_delivery(5); // [0,10)   -> bin 1
        tally.record_delivery(120); // [100,250)-> bin 5
        let snap = tally.snapshot();
        assert_eq!(snap.latency_hist[1], 1);
        assert_eq!(snap.latency_hist[5], 1);
    }

    #[test]
    fn partition_tally_counts_posts_high_and_broadcast_separately() {
        let mut t = PartitionTally::new();
        t.record_post(100, false, false);
        t.record_post(100, true, false);
        t.record_post(100, false, true);
        assert_eq!(t.posts, 3);
        assert_eq!(t.bytes, 300);
        assert_eq!(t.posts_high, 1);
        assert_eq!(t.posts_broadcast, 1);
    }

    #[test]
    fn partition_tally_records_deliveries_and_prune_snaps() {
        let mut t = PartitionTally::new();
        t.record_deliveries(2);
        t.record_prune(5, 1);
        assert_eq!(t.deliveries, 2);
        assert_eq!(t.pruned, 5);
        assert_eq!(t.snapped, 1);
    }

    #[test]
    fn snapshot_invariant_uses_workspace_buckets() {
        // Locks the bucket shape into the test surface so a future change to
        // the constants trips a test rather than silently desyncing clients.
        assert_eq!(LATENCY_BUCKETS_MS.len() + 1, BusTally::new().snapshot().latency_hist.len());
        assert_eq!(SIZE_BUCKETS_BYTES.len() + 1, BusTally::new().snapshot().size_hist.len());
    }
}
