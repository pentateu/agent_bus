# agent-bus Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A live, full-screen terminal dashboard — `agent-bus dashboard` — that polls the daemon once a second and renders throughput, delivery latency, message-size distribution, backlog gauges, and per-partition tables. The dashboard owns its rolling one-hour series; the daemon owns cumulative counters and histograms since its own start.

**Architecture:** The daemon gains a `BusTally` (cross-partition histograms) held inside `BusState` and a `PartitionTally` (per-partition cumulative counters) held inside each `Partition`, both bumped inside the existing `publish` / `deliver` / `prune` code paths that already run under the state mutex. Two `Arc<AtomicU64>` counters for active waiters/followers are created in `serve()` and passed to the connection tasks, because those connections park outside the mutex. A new `Request::Metrics` / `Response::Metrics` carries one `MetricsReport` snapshot per poll. The CLI gains a `dashboard` module: a poll thread that issues blocking `Client::request` calls and a UI thread that runs the ratatui event loop; they cross on a std channel. The App state and render are pure functions so they can be unit-tested without a terminal.

**Tech Stack:** Unchanged workspace plus `ratatui = "0.29"` and `crossterm = "0.28"` added to `crates/cli` only.

**Reference:** The design spec is at `docs/specs/2026-08-07-dashboard.md`. Read it before starting. The agent-bus design at `docs/specs/2026-08-06-agent-bus-design.md` is also assumed.

---

## Key Domain Concepts

Read this before Task 2. The engineer is assumed to know Rust and have read the agent-bus design, but nothing about this feature.

**Tally.** A running accumulator. The daemon keeps two kinds: a per-partition `PartitionTally` (counters: posts, deliveries, bytes, posts_high, posts_broadcast, pruned, snapped) and a single cross-partition `BusTally` (two histograms: post-to-delivery latency in ms, and message body size in bytes). Both are bumped inside code paths that already hold the state mutex, so they add no locking.

**Bucketing.** A fixed array of lower-bound boundaries; a value falls into bin `i` when it is `>= buckets[i]` and `< buckets[i+1]`, with the final bin being `>= buckets[last]`. The number of histogram cells is always `boundaries.len() + 1` — the first cell is `< buckets[0]` (empty, because the first boundary is always 0) and the last cell is `>= buckets[last]`. `bin_index(buckets, v) = buckets.iter().filter(|&&b| b <= v).count()`. Latency boundaries: `[0, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000, 60000]` ms. Size boundaries: `[0, 32, 64, 128, 256, 512, 1024, 4096, 16384, 65536, 262144, 1048576, 4194304]` bytes. They are sent on every response so a client never hardcodes them.

**Latency is post-to-first-delivery.** `delivery_now_ms - msg.id.timestamp_ms()`. Recorded once per message *delivered*, which for broadcast means once per consumer label that receives it (so "how long until each agent picked it up"). `msg.id.timestamp_ms()` is the ULID's embedded millisecond timestamp, already used by `Message::age_secs`.

**Cumulative on the daemon, deltas on the dashboard.** The daemon never decrements a tally; the dashboard computes `rate = cur - prev` per poll. A daemon that exits and comes back reports fresh zeros; the dashboard notices `uptime_secs` went backwards (or the pid changed), drops that one diff so the fresh zeros do not register as a giant negative spike, and continues.

**Rebaseline.** When a poll's `uptime_secs < prev_uptime` (or `pid` changed), the dashboard clears `prev_*`, returns `None` from the diff, tags `last_restart = now`, and shows a "daemon restarted" marker. The rolling series is preserved; only the one bucket is annotated.

---

## File Structure

```
agent-bus/
├── crates/
│   ├── protocol/src/lib.rs        +Request::Metrics, +Response::Metrics,
│                                   +MetricsReport, MetricsTotals,
│                                   PartitionMetrics, TopicCount, ConsumerLag
│   ├── daemon/src/
│   │   ├── main.rs                unchanged
│   │   ├── metrics.rs              NEW: bucket consts, bin_index, BusTally,
│   │   │                           PartitionTally, with unit tests
│   │   ├── state.rs                +BusTally field
│   │   ├── partition.rs            +PartitionTally field, bump in
│   │   │                           publish/deliver/prune
│   │   ├── handler.rs              +build_metrics, +Request::Metrics arm
│   │   └── server.rs               +active_waiters/active_followers
│   │                                Arc<AtomicU64>, threaded into
│   │                                wait_for_message/follow and into dispatch
│   └── cli/src/
│       ├── cli.rs                  +Dashboard subcommand + parse_refresh
│       ├── main.rs                  +arm for Dashboard
│       ├── Cargo.toml              +ratatui, +crossterm
│       └── dashboard/
│           ├── mod.rs              public surface, DashboardOpts, run()
│           ├── sample.rs           Report polling, Sample::diff, rolling
│           │                        Series, rebaseline; unit-tested
│           ├── app.rs              pure App state + App::tick; unit-tested
│           └── ui.rs               pure render into ratatui::Frame
└── crates/cli/tests/integration.rs +metrics integration test
```

---

## Task 1: Protocol types

**Files:**
- Modify: `crates/protocol/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/protocol/src/lib.rs`:

```rust
    #[test]
    fn metrics_request_roundtrips() {
        let req = Request::Metrics;
        let line = encode(&req).unwrap();
        assert_eq!(decode::<Request>(&line).unwrap(), req);
    }

    #[test]
    fn metrics_report_roundtrips() {
        let report = MetricsReport {
            pid: 42,
            uptime_secs: 300,
            poll_at_ms: 1_700_000_000_000,
            totals: MetricsTotals {
                posts: 5, deliveries: 4, bytes_posted: 900,
                posts_high: 1, posts_broadcast: 2,
                pruned: 0, skipped: 0, snapped: 0,
            },
            latency_buckets_ms: vec![0, 10, 25],
            latency_histogram_ms: vec![3, 1, 0, 0],
            size_buckets_bytes: vec![0, 64, 256],
            size_histogram_bytes: vec![1, 3, 1, 0],
            active_waiters: 1,
            active_followers: 0,
            partitions: vec![PartitionMetrics {
                name: "iot_base".to_owned(),
                message_count: 2,
                oldest_age_secs: Some(30),
                undelivered_lag: 1,
                participants: 2,
                posts: 5, deliveries: 4, bytes: 900,
                pruned: 0, skipped: 0, snapped: 0,
            }],
            publishers: vec!["dev_01".to_owned()],
            top_topics: vec![TopicCount {
                topic: "iot_base/dev_01".to_owned(),
                count: 4, total_bytes: 800,
            }],
            top_consumers: vec![ConsumerLag {
                label: "reviewer_01".to_owned(),
                partition: "iot_base".to_owned(),
                pattern: "iot_base/**".to_owned(),
                broadcast: false, lag: 1,
            }],
        };
        let res = Response::Metrics { metrics: report.clone() };
        let line = encode(&res).unwrap();
        assert_eq!(decode::<Response>(&line).unwrap(), res);
    }

    #[test]
    fn histogram_has_one_more_cell_than_boundaries() {
        // A documented invariant the daemon is expected to uphold; a client
        // that wanted to validate could check it on decode, so the type itself
        // does not enforce a length and the test just documents the shape.
        let report = sample_report_for_shape_test();
        assert_eq!(report.latency_histogram_ms.len(), report.latency_buckets_ms.len() + 1);
        assert_eq!(report.size_histogram_bytes.len(), report.size_buckets_bytes.len() + 1);
    }

    fn sample_report_for_shape_test() -> MetricsReport {
        MetricsReport {
            pid: 0, uptime_secs: 0, poll_at_ms: 0,
            totals: MetricsTotals::default(),
            latency_buckets_ms: vec![0, 10], latency_histogram_ms: vec![0, 0, 0],
            size_buckets_bytes: vec![0, 64], size_histogram_bytes: vec![0, 0, 0],
            active_waiters: 0, active_followers: 0,
            partitions: vec![], publishers: vec![],
            top_topics: vec![], top_consumers: vec![],
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p agent-bus-protocol 2>&1 | tail -10`
Expected: compile errors — `cannot find variant Request::Metrics`, `cannot find type MetricsReport`.

- [ ] **Step 3: Add the types**

In `crates/protocol/src/lib.rs`, add the variant to `Request` (between `Status` and `Stop`):

```rust
    /// Cumulative counters + histograms since the daemon started, for the
    /// dashboard. One snapshot per poll; the caller diffs successive snapshots
    /// into a time series.
    Metrics,
```

Add the variant to `Response` (after `Status`):

```rust
    Metrics {
        metrics: MetricsReport,
    },
```

Add the new types near `StatusReport`:

```rust
/// Cumulative daemon metrics since daemon start, for `agent-bus dashboard`.
///
/// All counters and histograms are monotonically non-decreasing for the life
/// of one daemon process. A client that polls repeatedly derives rates by
/// differencing successive snapshots; a daemon restart resets everything and
/// the client must rebaseline (see the design spec).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsReport {
    pub pid: u32,
    pub uptime_secs: u64,
    /// Wall-clock milliseconds at the daemon when this snapshot was built.
    /// Lets the dashboard render "x seconds ago" without trusting its own
    /// clock for cross-process comparisons.
    pub poll_at_ms: u64,
    pub totals: MetricsTotals,
    /// Lower bounds of each latency bin. Always begins with 0; the histogram
    /// has one more cell than this (the last is `>=` the final boundary).
    pub latency_buckets_ms: Vec<u64>,
    pub latency_histogram_ms: Vec<u64>,
    pub size_buckets_bytes: Vec<u64>,
    pub size_histogram_bytes: Vec<u64>,
    /// Connections currently parked in `wait`. Instantaneous, not cumulative.
    pub active_waiters: u64,
    /// Connections currently streaming via `follow`.
    pub active_followers: u64,
    pub partitions: Vec<PartitionMetrics>,
    /// Distinct `from` values across all retained logs, sorted.
    pub publishers: Vec<String>,
    /// Concrete topics in the retained window, top 10 by count.
    pub top_topics: Vec<TopicCount>,
    /// Per-position lag, top 10 by lag descending.
    pub top_consumers: Vec<ConsumerLag>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MetricsTotals {
    pub posts: u64,
    pub deliveries: u64,
    pub bytes_posted: u64,
    pub posts_high: u64,
    pub posts_broadcast: u64,
    pub pruned: u64,
    /// Corrupt log lines skipped at load. Comes from `PartitionLog`; not a
    /// runtime increment, surfaced for visibility.
    pub skipped: u64,
    pub snapped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionMetrics {
    pub name: String,
    pub message_count: usize,
    pub oldest_age_secs: Option<u64>,
    /// Unread messages behind every position in this partition.
    pub undelivered_lag: usize,
    /// Distinct `from` values in this partition's retained log.
    pub participants: usize,
    pub posts: u64,
    pub deliveries: u64,
    pub bytes: u64,
    pub pruned: u64,
    pub skipped: u64,
    pub snapped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicCount {
    pub topic: String,
    pub count: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumerLag {
    pub label: String,
    pub partition: String,
    pub pattern: String,
    pub broadcast: bool,
    pub lag: usize,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p agent-bus-protocol 2>&1 | tail -10`
Expected: `test result: ok. 9 passed; 0 failed` (the existing 6 plus 3 new).

- [ ] **Step 5: Lint**

Run: `cargo clippy -p agent-bus-protocol --all-targets 2>&1 | tail -10`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/protocol
git commit -m "feat(protocol): metrics report wire types for the dashboard"
```

---

## Task 2: Daemon tallies and bucketing

**Files:**
- Create: `crates/daemon/src/metrics.rs`
- Modify: `crates/daemon/src/main.rs` (declare the module)
- Modify: `crates/daemon/src/state.rs` (carry a `BusTally`)
- Modify: `crates/daemon/src/partition.rs` (carry a `PartitionTally`)

- [ ] **Step 1: Write the failing tests**

Create `crates/daemon/src/metrics.rs` containing only this test module at the bottom:

```rust
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
        tally.record_post(50);    // [0,32)  -> bin 1
        tally.record_post(40);    // [32,64) -> bin 2
        tally.record_post(1024);  // [1024,4096) -> bin 6
        let snap = tally.snapshot();
        assert_eq!(snap.size_hist[1], 1);
        assert_eq!(snap.size_hist[2], 1);
        assert_eq!(snap.size_hist[6], 1);
        assert_eq!(snap.size_hist.iter().sum::<u64>(), 3);
    }

    #[test]
    fn record_delivery_increments_latency_histogram() {
        let mut tally = BusTally::new();
        tally.record_delivery(5);    // [0,10)   -> bin 1
        tally.record_delivery(120);  // [100,250)-> bin 5
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
        assert_eq!(t.posts_broadcast, 2);
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p agent-bus-daemon metrics 2>&1 | tail -10`
Expected: compile errors — `cannot find function bin_index`, `cannot find type BusTally`.

- [ ] **Step 3: Implement `metrics.rs`**

Prepend to `crates/daemon/src/metrics.rs`:

```rust
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
    &[0, 32, 64, 128, 256, 512, 1024, 4096, 16384, 65536, 262144, 1_048_576, 4_194_304];

/// Index of the histogram bin a value falls into.
///
/// `boundaries` are the lower bounds; the histogram has `boundaries.len() + 1`
/// cells where cell 0 is `< boundaries[0]` and the final cell is `>=` the last
/// boundary. A value equal to a boundary goes into the bin that STARTS at that
/// boundary: 10ms lands in the `[10,25)` bin, not the `[0,10)` bin.
#[must_use]
pub fn bin_index(boundaries: &[u64], value: u64) -> usize {
    // Count of boundaries `<= value` is exactly the bin index: every boundary
    // at or below the value opens a bin that the value does NOT fall into, and
    // every boundary above the value opens a bin that it does.
    boundaries.iter().filter(|&&b| b <= value).count()
}

/// Cross-partition histograms, owned by [`crate::state::BusState`].
#[derive(Debug, Clone)]
pub struct BusTally {
    latency_hist: Vec<u64>,
    size_hist: Vec<u64>,
}

/// A cheap snapshot of both histograms for `build_metrics`.
pub struct TallySnapshot<'a> {
    pub latency_hist: &'a [u64],
    pub size_hist: &'a [u64],
}

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
```

- [ ] **Step 4: Declare the module and thread the tallies**

In `crates/daemon/src/main.rs`, add `mod metrics;` next to the other module declarations.

In `crates/daemon/src/state.rs`, add a field and constructor line:

```rust
use crate::metrics::BusTally;
// ...
pub struct BusState {
    state_dir: PathBuf,
    partitions: BTreeMap<String, Partition>,
    policy: RetentionPolicy,
    started: Instant,
    last_activity: Instant,
    pub bus_tally: BusTally,
}
// in BusState::new, add the field:
    Self { state_dir, partitions: BTreeMap::new(), policy, started: now, last_activity: now, bus_tally: BusTally::new() }
```

Add an accessor:

```rust
    #[must_use]
    pub fn bus_tally(&self) -> &BusTally { &self.bus_tally }
```

In `crates/daemon/src/partition.rs`, add a field:

```rust
use crate::metrics::PartitionTally;
// inside struct Partition:
    tally: PartitionTally,
// in Partition::open, initial field:
    tally: PartitionTally::new(),
// public accessor:
    #[must_use]
    pub fn tally(&self) -> &PartitionTally { &self.tally }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p agent-bus-daemon metrics 2>&1 | tail -10`
Expected: `test result: ok. 7 passed; 0 failed`.

Then build the daemon so the field additions compile: `cargo build -p agent-bus-daemon 2>&1 | tail -5`. Expected: `Finished`. The tallies are not yet bumped; that comes in Task 3.

- [ ] **Step 6: Lint**

Run: `cargo clippy -p agent-bus-daemon --all-targets 2>&1 | tail -10`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/daemon
git commit -m "feat(daemon): cumulative tally and histogram bucketing"
```

---

## Task 3: Wire instrumentation into publish / deliver / prune

**Files:**
- Modify: `crates/daemon/src/partition.rs`
- Modify: `crates/daemon/src/server.rs` (active waiter/follower atomics)

- [ ] **Step 1: Write the failing test for partition instrumentation**

Append to the `tests` module in `crates/daemon/src/partition.rs`:

```rust
    #[test]
    fn publish_increments_partition_tally() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "x")).unwrap();
        p.publish(msg("iot_base/dev_01", "y")).unwrap();
        assert_eq!(p.tally().posts, 2);
        assert_eq!(p.tally().bytes, 2);
        assert_eq!(p.tally().deliveries, 0);
    }

    #[test]
    fn deliver_increments_delivery_count() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        p.publish(msg("iot_base/dev_01", "a")).unwrap();
        p.publish(msg("iot_base/dev_01", "b")).unwrap();
        let pattern = Pattern::parse("iot_base/**").unwrap();
        p.deliver(&pattern, "r", usize::MAX).unwrap();
        assert_eq!(p.tally().deliveries, 2);
        // A second deliver finds nothing, so it must not bump again.
        p.deliver(&pattern, "r", usize::MAX).unwrap();
        assert_eq!(p.tally().deliveries, 2);
    }

    #[test]
    fn prune_records_removed_and_snapped_into_tally() {
        let dir = TempDir::new().unwrap();
        let mut p = partition(&dir);
        let old = msg("iot_base/dev_01", "old");
        p.publish(old).unwrap();
        let fresh = msg("iot_base/dev_01", "fresh");
        p.publish(fresh).unwrap();

        // Put a stale cursor in place so snap_forward fires.
        let pattern = Pattern::parse("iot_base/**").unwrap();
        p.deliver(&pattern, "r", 1).unwrap();    // consumes "old", sets cursor
        let now = fresh.id.timestamp_ms() / 1000;
        p.prune(&RetentionPolicy { max_age_secs: 1 }, now + 10).unwrap();
        // Both messages age out under the 1s window.
        assert_eq!(p.tally().pruned, 2);
        // The cursor pointed at `old`, which is pruned, so it gets snapped.
        assert_eq!(p.tally().snapped, 1);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p agent-bus-daemon partition::tests 2>&1 | tail -10`
Expected: assertion failures — `p.tally().posts` is 0.

- [ ] **Step 3: Bump the tally inside `Partition`**

In `crates/daemon/src/partition.rs`, modify `publish`. The publish path records
size on the bus tally via the handler (Task 4 records latency on delivery);
`publish` only bumps `PartitionTally`:

```rust
    pub fn publish(&mut self, message: Message) -> Result<Ulid> {
        let id = message.id;
        let body_bytes = u64::try_from(message.body.len()).unwrap_or(u64::MAX);
        self.tally.record_post(body_bytes, message.priority == Priority::High, message.broadcast);
        self.log.append(&message)?;
        let _ = self.notify.send(());
        Ok(id)
    }
```

Modify `deliver` — after `merged` is built and before returning, add:

```rust
        let delivered = u64::try_from(merged.len()).unwrap_or(u64::MAX);
        self.tally.record_deliveries(delivered);
```

Modify `prune` — after `snap_forward` returns, bump the tally. Replace the
existing block so the counter grows with the count, not just the keys:

```rust
        if outcome.removed > 0
            && let Some(oldest) = outcome.oldest_surviving
        {
            let snapped_keys = self.cursors.snap_forward(oldest);
            let snapped_count = u64::try_from(snapped_keys.len()).unwrap_or(u64::MAX);
            if !snapped_keys.is_empty() {
                for key in snapped_keys {
                    if !self.snapped.contains(&key) {
                        self.snapped.push(key);
                    }
                }
                self.persist_cursors()?;
            }
            self.tally.record_prune(u64::try_from(outcome.removed).unwrap_or(u64::MAX), snapped_count);
        } else {
            // A prune that removed nothing still records so `pruned` stays a
            // faithful cumulative count; removed==0 here, so this adds zero.
            self.tally.record_prune(u64::try_from(outcome.removed).unwrap_or(u64::MAX), 0);
        }
```

- [ ] **Step 4: Add active-waiter / active-follower atomics to the server**

In `crates/daemon/src/server.rs`, import atomics:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
```

In `serve()`, create and thread two counters:

```rust
    let active_waiters = Arc::new(AtomicU64::new(0));
    let active_followers = Arc::new(AtomicU64::new(0));
```

Pass clones into each spawned connection task alongside `state` and `shutdown`:

```rust
                        let state = Arc::clone(&state);
                        let shutdown = shutdown_tx.clone();
                        let socket_path = socket.to_path_buf();
                        let aw = Arc::clone(&active_waiters);
                        let af = Arc::clone(&active_followers);
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_connection(stream, state, shutdown, socket_path, aw, af).await
                            {
                                eprintln!("agent-bus: connection error: {e:#}");
                            }
                        });
```

Change `handle_connection`, `wait_for_message`, and `follow` signatures to accept `active_waiters: Arc<AtomicU64>` and `active_followers: Arc<AtomicU64>`.

A small RAII guard makes the bookkeeping hard to get wrong across many return paths:

```rust
struct ActiveGuard {
    counter: Arc<AtomicU64>,
}

impl ActiveGuard {
    fn new(counter: Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}
```

In `wait_for_message`, take the guard at the top:

```rust
    let _waiter_guard = ActiveGuard::new(Arc::clone(&active_waiters));
    // existing body unchanged
```

In `follow`, take the guard at the top:

```rust
    let _follower_guard = ActiveGuard::new(Arc::clone(&active_followers));
    // existing body unchanged
```

Because the guard decrements on `Drop`, every `?` and early return path is
covered automatically.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p agent-bus-daemon 2>&1 | tail -15`
Expected: all tests pass, including the new partition tests. The existing
`server::tests` helpers call `wait_for_message` and `follow` directly; add
throwaway `Arc::new(AtomicU64::new(0))` arguments to those call sites (they
do not assert on active counts).

- [ ] **Step 6: Lint**

Run: `cargo clippy -p agent-bus-daemon --all-targets 2>&1 | tail -10`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/daemon
git commit -m "feat(daemon): instrument publish/deliver/prune and track active waiters"
```

---

## Task 4: `Request::Metrics` dispatch and `build_metrics`

**Files:**
- Modify: `crates/daemon/src/handler.rs`
- Modify: `crates/daemon/src/server.rs` (pass counters into `dispatch`, record latency in `follow`)

- [ ] **Step 1: Write the failing test**

Append a tests module to `crates/daemon/src/handler.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    #[test]
    fn build_metrics_reports_cumulative_counts_after_activity() {
        let dir = TempDir::new().unwrap();
        let mut state = BusState::new(dir.path().to_path_buf());
        let name = PartitionName::parse("iot_base").unwrap();
        let topic = Topic::parse("iot_base/dev_01").unwrap();

        for body in ["one", "two", "three"] {
            let m = Message::new(topic.clone(), body.to_owned(), Priority::Normal, None);
            state.partition_mut(&name).unwrap().publish(m).unwrap();
        }
        // Deliver one so deliveries > 0.
        let pattern = Pattern::parse("iot_base/**").unwrap();
        let _ = state.partition_mut(&name).unwrap().deliver(&pattern, "rev", 1).unwrap();

        let aw = AtomicU64::new(0);
        let af = AtomicU64::new(0);
        let report = build_metrics(&state, &aw, &af);
        assert_eq!(report.totals.posts, 3);
        assert_eq!(report.totals.deliveries, 1);
        assert_eq!(report.totals.bytes_posted, 6); // "one"+"two"+"three"
        assert_eq!(report.partitions.len(), 1);
        assert_eq!(report.partitions[0].name, "iot_base");
        assert_eq!(report.partitions[0].participants, 1);
        assert!(report.latency_histogram_ms.iter().sum::<u64>() >= 1);
    }

    #[test]
    fn build_metrics_active_counters_reflect_the_instantaneous_state() {
        let dir = TempDir::new().unwrap();
        let state = BusState::new(dir.path().to_path_buf());
        let aw = AtomicU64::new(2);
        let af = AtomicU64::new(0);
        let report = build_metrics(&state, &aw, &af);
        assert_eq!(report.active_waiters, 2);
        assert_eq!(report.active_followers, 0);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p agent-bus-daemon build_metrics 2>&1 | tail -10`
Expected: `cannot find function build_metrics`.

- [ ] **Step 3: Implement `build_metrics` and record latency on delivery**

`Partition` does not own `BusTally`, so the handler records delivery latency
into `bus_tally` whenever a `deliver` call returns messages. Add a helper on
`BusState`:

```rust
    /// Record one delivered message's latency into the cross-partition histogram.
    pub fn record_delivery_latency(&mut self, post_ms: u64) {
        let now_ms = crate::handler::now_millis();
        self.bus_tally.record_delivery(now_ms.saturating_sub(post_ms));
    }
```

Add the `now_millis` helper (used by both `build_metrics` and the helper above)
in `handler.rs`:

```rust
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};
use agent_bus_protocol::{
    ConsumerLag, MetricsReport, MetricsTotals, PartitionMetrics, TopicCount,
};
use crate::metrics::{LATENCY_BUCKETS_MS, SIZE_BUCKETS_BYTES};

#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
```

In `dispatch`, after every `deliver` call that returns messages, record the
latency for each returned message into `state.bus_tally`:

- `Request::Wait` `Ok(batch) =>` arm — loop over `batch` and call
  `state.record_delivery_latency(m.id.timestamp_ms())`.
- `Request::Read` — same for `messages`.
- `Request::FollowPending` — the actual `deliver` happens in `server::follow`;
  inside the `follow` loop after `deliver` returns the batch, lock the state
  briefly to record:

```rust
        if !batch.is_empty() {
            {
                let mut guard = state.lock().await;
                for m in &batch {
                    guard.record_delivery_latency(m.id.timestamp_ms());
                }
            }
            send(write_half, &Response::Messages { messages: batch }).await?;
            continue;
        }
```

Implement `build_metrics`:

```rust
pub fn build_metrics(
    state: &BusState,
    active_waiters: &AtomicU64,
    active_followers: &AtomicU64,
) -> MetricsReport {
    let tally = state.bus_tally();
    let t_snap = tally.snapshot();

    let mut totals = MetricsTotals::default();
    let mut partitions = Vec::new();
    let mut all_publishers = std::collections::BTreeSet::new();
    let mut all_topics: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
    let now = now_secs();

    for p in state.partitions() {
        let p_tally = p.tally();
        totals.posts += p_tally.posts;
        totals.deliveries += p_tally.deliveries;
        totals.bytes_posted += p_tally.bytes;
        totals.posts_high += p_tally.posts_high;
        totals.posts_broadcast += p_tally.posts_broadcast;
        totals.pruned += p_tally.pruned;
        totals.snapped += p_tally.snapped;
        totals.skipped += u64::try_from(p.skipped_records()).unwrap_or(u64::MAX);

        let mut publishers = std::collections::BTreeSet::new();
        let mut lag = 0usize;
        for m in p.log_messages() {
            publishers.insert(m.from.clone());
            all_publishers.insert(m.from.clone());
            let entry = all_topics
                .entry(m.topic.as_str().to_owned())
                .or_insert((0, 0));
            entry.0 += 1;
            entry.1 += u64::try_from(m.body.len()).unwrap_or(u64::MAX);
        }
        for snap in p.pattern_snapshots() {
            lag += snap.lag;
        }

        partitions.push(PartitionMetrics {
            name: p.name().to_owned(),
            message_count: p.message_count(),
            oldest_age_secs: p.oldest_age_secs(now),
            undelivered_lag: lag,
            participants: publishers.len(),
            posts: p_tally.posts,
            deliveries: p_tally.deliveries,
            bytes: p_tally.bytes,
            pruned: p_tally.pruned,
            skipped: u64::try_from(p.skipped_records()).unwrap_or(u64::MAX),
            snapped: p_tally.snapped,
        });
    }

    let mut top_topics: Vec<TopicCount> = all_topics
        .into_iter()
        .map(|(topic, (count, total_bytes))| TopicCount { topic, count, total_bytes })
        .collect();
    top_topics.sort_by(|a, b| b.count.cmp(&a.count).then(b.total_bytes.cmp(&a.total_bytes)));
    top_topics.truncate(10);

    let mut top_consumers: Vec<ConsumerLag> = Vec::new();
    for p in state.partitions() {
        for snap in p.pattern_snapshots() {
            top_consumers.push(ConsumerLag {
                label: snap.label,
                partition: p.name().to_owned(),
                pattern: snap.key,
                broadcast: snap.broadcast,
                lag: snap.lag,
            });
        }
    }
    top_consumers.sort_by(|a, b| b.lag.cmp(&a.lag));
    top_consumers.truncate(10);

    MetricsReport {
        pid: std::process::id(),
        uptime_secs: state.uptime_secs(),
        poll_at_ms: now_millis(),
        totals,
        latency_buckets_ms: LATENCY_BUCKETS_MS.to_vec(),
        latency_histogram_ms: t_snap.latency_hist.to_vec(),
        size_buckets_bytes: SIZE_BUCKETS_BYTES.to_vec(),
        size_histogram_bytes: t_snap.size_hist.to_vec(),
        active_waiters: active_waiters.load(Ordering::Relaxed),
        active_followers: active_followers.load(Ordering::Relaxed),
        partitions,
        publishers: all_publishers.into_iter().collect(),
        top_topics,
        top_consumers,
    }
}
```

Add `pub fn log_messages(&self) -> impl Iterator<Item = &Message> { self.log.messages().iter() }` to `Partition`.

- [ ] **Step 4: Dispatch `Request::Metrics`**

Change the `dispatch` signature:

```rust
pub fn dispatch(
    state: &mut BusState,
    request: Request,
    active_waiters: &AtomicU64,
    active_followers: &AtomicU64,
) -> Dispatch {
```

Add an arm before `Request::Stop`:

```rust
        Request::Metrics => {
            let report = build_metrics(state, active_waiters, active_followers);
            Dispatch::Reply(Response::Metrics { metrics: report })
        }
```

- [ ] **Step 5: Update the server to pass the atomics into `dispatch`**

In `handle_connection`, the call site becomes:

```rust
        let outcome = {
            let mut guard = state.lock().await;
            dispatch(&mut guard, request, &active_waiters, &active_followers)
        };
```

`active_waiters` and `active_followers` are already arguments of `handle_connection` (added in Task 3).

- [ ] **Step 6: Update existing server tests**

Tests in `server::tests` that call `wait_for_message` / `follow` were updated in Task 3 to pass throwaway atomics. If any calls `dispatch` directly, add the two atomic arguments (`&AtomicU64::new(0)` and `&AtomicU64::new(0)`).

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p agent-bus-daemon 2>&1 | tail -10`
Expected: all pass.

Run: `cargo test --workspace 2>&1 | tail -10`
Expected: all pass (the CLI integration tests do not yet exercise Metrics; they still build).

- [ ] **Step 8: Lint**

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -10`
Expected: no warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/daemon
git commit -m "feat(daemon): dispatch Request::Metrics and assemble MetricsReport"
```

---

## Task 5: Dashboard sampling layer

**Files:**
- Create: `crates/cli/src/dashboard/mod.rs`
- Create: `crates/cli/src/dashboard/sample.rs`
- Modify: `crates/cli/src/main.rs` (declare `mod dashboard;`)

- [ ] **Step 1: Write the failing tests**

Create `crates/cli/src/dashboard/sample.rs` with this test module at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use agent_bus_protocol::{MetricsReport, MetricsTotals};

    fn report(posts: u64, deliveries: u64, uptime: u64, pid: u32) -> MetricsReport {
        MetricsReport {
            pid, uptime_secs: uptime, poll_at_ms: 0,
            totals: MetricsTotals { posts, deliveries, bytes_posted: 0,
                posts_high: 0, posts_broadcast: 0, pruned: 0, skipped: 0, snapped: 0 },
            latency_buckets_ms: vec![0, 10], latency_histogram_ms: vec![0, 0, 0],
            size_buckets_bytes: vec![0, 64], size_histogram_bytes: vec![0, 0, 0],
            active_waiters: 0, active_followers: 0,
            partitions: vec![], publishers: vec![], top_topics: vec![], top_consumers: vec![],
        }
    }

    #[test]
    fn diff_yields_per_second_rate() {
        let prev = report(10, 8, 100, 1);
        let cur  = report(13, 9, 101, 1);
        let d = Sample::diff(&prev, &cur, 1).expect("same daemon");
        assert_eq!(d.posts_per_sec, 3.0);
        assert_eq!(d.deliveries_per_sec, 1.0);
    }

    #[test]
    fn diff_over_a_longer_interval_divides_by_elapsed_seconds() {
        let prev = report(10, 8, 100, 1);
        let cur  = report(40, 18, 110, 1); // 10s elapsed
        let d = Sample::diff(&prev, &cur, 10).expect("same daemon");
        assert_eq!(d.posts_per_sec, 3.0);
        assert_eq!(d.deliveries_per_sec, 1.0);
    }

    #[test]
    fn a_daemon_restart_returns_none() {
        let prev = report(10, 8, 500, 1);
        let cur  = report(0, 0, 2, 2); // fresh daemon, pid changed, uptime shrank
        assert!(Sample::diff(&prev, &cur, 1).is_none());
    }

    #[test]
    fn percentile_from_histogram_matches_brute_force() {
        // 9 deliveries: 4 at 5ms, 3 at 15ms, 2 at 60ms.
        let mut hist = vec![0u64; LATENCY_BUCKETS_MS.len() + 1];
        let buckets = LATENCY_BUCKETS_MS.to_vec();
        for v in [5u64, 5, 5, 5, 15, 15, 15, 60, 60] {
            hist[bin_index(&buckets, v)] += 1;
        }
        // p50 of 9 samples (sorted): the 5th value -> 15ms -> [10,25) bin.
        assert_eq!(percentile_bucket(&hist, 0.5), bin_index(&buckets, 15));
        // p95 of 9 samples: the 9th value -> 60ms -> [50,100) bin.
        assert_eq!(percentile_bucket(&hist, 0.95), bin_index(&buckets, 60));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p agent-bus-cli sample 2>&1 | tail -10`
Expected: `cannot find function Sample::diff` / `cannot find type Sample`.

- [ ] **Step 3: Implement `sample.rs`**

Prepend to `crates/cli/src/dashboard/sample.rs`:

```rust
//! Per-second diffing of two `MetricsReport`s, plus percentile helpers.
//!
//! The dashboard keeps the previous report and calls [`Sample::diff`] each
//! poll. The diff is the rate over the elapsed interval and the per-second
//! histogram delta, so "p95 in the last minute" is computed from a minute of
//! these deltas rather than from the daemon's all-time histogram.

use agent_bus_protocol::MetricsReport;

/// Latency bucket boundaries, mirrored from the daemon. The daemon sends the
/// same array on every response, so a client may use those instead; this
/// constant exists so tests do not have to construct a full report just to
/// get bucket boundaries.
pub const LATENCY_BUCKETS_MS: &[u64] =
    &[0, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000, 60000];

#[must_use]
pub fn bin_index(boundaries: &[u64], value: u64) -> usize {
    boundaries.iter().filter(|&&b| b <= value).count()
}

/// One poll's worth of derived numbers, suitable for appending to a series.
pub struct Diff {
    pub posts_per_sec: f64,
    pub deliveries_per_sec: f64,
    pub bytes_per_sec: f64,
    /// Latency histogram for THIS interval only (delta of the cumulative).
    pub latency_delta: Vec<u64>,
    pub size_delta: Vec<u64>,
    /// p95 of this interval, in ms (the bucket's lower bound — good enough for
    /// a sparkline; the UI rounds to the bucket label).
    pub p95_latency_ms: u64,
    pub avg_latency_ms: u64,
    pub avg_size_bytes: u64,
}

/// Diff two reports. Returns `None` if the daemon restarted between them
/// (uptime shrank or pid changed), in which case the caller drops the diff
/// and tags a rebaseline.
pub struct Sample;
impl Sample {
    #[must_use]
    pub fn diff(prev: &MetricsReport, cur: &MetricsReport, elapsed_secs: u64) -> Option<Diff> {
        if cur.uptime_secs < prev.uptime_secs || cur.pid != prev.pid {
            return None;
        }
        let secs = if elapsed_secs == 0 { 1 } else { elapsed_secs };
        let f = |a: u64, b: u64| -> f64 { (a.saturating_sub(b) as f64) / secs as f64 };

        let latency_delta: Vec<u64> = cur
            .latency_histogram_ms
            .iter()
            .zip(prev.latency_histogram_ms.iter())
            .map(|(c, p)| c.saturating_sub(*p))
            .collect();
        let size_delta: Vec<u64> = cur
            .size_histogram_bytes
            .iter()
            .zip(prev.size_histogram_bytes.iter())
            .map(|(c, p)| c.saturating_sub(*p))
            .collect();

        let total_lat_samples: u64 = latency_delta.iter().sum();
        let avg_latency_ms = if total_lat_samples == 0 {
            0
        } else {
            weighted_mid(&latency_delta, &cur.latency_buckets_ms) / total_lat_samples
        };
        let p95_latency_ms = if total_lat_samples == 0 {
            0
        } else {
            cur.latency_buckets_ms
                .get(percentile_bucket(&latency_delta, 0.95))
                .copied()
                .unwrap_or(0)
        };

        let total_size_samples: u64 = size_delta.iter().sum();
        let avg_size_bytes = if total_size_samples == 0 {
            0
        } else {
            weighted_mid(&size_delta, &cur.size_buckets_bytes) / total_size_samples
        };

        Some(Diff {
            posts_per_sec: f(cur.totals.posts, prev.totals.posts),
            deliveries_per_sec: f(cur.totals.deliveries, prev.totals.deliveries),
            bytes_per_sec: f(cur.totals.bytes_posted, prev.totals.bytes_posted),
            latency_delta,
            size_delta,
            p95_latency_ms,
            avg_latency_ms,
            avg_size_bytes,
        })
    }
}

/// Index of the bucket holding the `q`-th percentile sample, given a histogram
/// `h`. `0 < q <= 1`. Uses nearest-rank: the sample at position
/// `ceil(q * total)`.
#[must_use]
pub fn percentile_bucket(histogram: &[u64], q: f64) -> usize {
    let total: u64 = histogram.iter().sum();
    if total == 0 {
        return 0;
    }
    let target = (q * total as f64).ceil() as u64;
    let target = target.clamp(1, total);
    let mut acc: u64 = 0;
    for (i, &c) in histogram.iter().enumerate() {
        acc += c;
        if acc >= target {
            return i;
        }
    }
    histogram.len() - 1
}

/// Sum of bucket midpoints weighted by each bucket's count, as a total (not
/// averaged); the caller divides by sample count for an average. Midpoints
/// approximate, which is the honest price of bucketing.
fn weighted_mid(histogram: &[u64], boundaries: &[u64]) -> u64 {
    let mut sum: u64 = 0;
    for (i, &count) in histogram.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let lo = boundaries.get(i).copied().unwrap_or(0);
        let hi = boundaries.get(i + 1).copied().unwrap_or(lo * 2 + 1);
        let mid = lo + (hi.saturating_sub(lo)) / 2;
        sum = sum.saturating_add(mid.saturating_mul(count));
    }
    sum
}
```

Create `crates/cli/src/dashboard/mod.rs`:

```rust
//! The dashboard: a live TUI view of agent-bus metrics.

pub mod sample;
```

Add `mod dashboard;` to `crates/cli/src/main.rs` (next to `mod commands;` etc.).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p agent-bus-cli sample 2>&1 | tail -10`
Expected: `test result: ok. 4 passed; 0 failed`.

- [ ] **Step 5: Lint**

Run: `cargo clippy -p agent-bus-cli --all-targets 2>&1 | tail -10`
Expected: no warnings (the `app` and `ui` modules do not exist yet; either stub them as empty in `mod.rs` or skip the lint until Task 6 — leave `mod.rs` as `pub mod sample;` only for now).

- [ ] **Step 6: Commit**

```bash
git add crates/cli
git commit -m "feat(cli): dashboard sample diffing and percentile helpers"
```

---

## Task 6: Dashboard App state and rendering

**Files:**
- Create: `crates/cli/src/dashboard/app.rs`
- Create: `crates/cli/src/dashboard/ui.rs`
- Modify: `crates/cli/src/dashboard/mod.rs`
- Modify: `crates/cli/Cargo.toml`

- [ ] **Step 1: Write the failing tests for `App::tick`**

Create `crates/cli/src/dashboard/app.rs` with this test module at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use agent_bus_protocol::{MetricsReport, MetricsTotals};

    fn report(posts: u64, deliveries: u64, uptime: u64, pid: u32) -> MetricsReport {
        MetricsReport {
            pid, uptime_secs: uptime, poll_at_ms: 0,
            totals: MetricsTotals { posts, deliveries, bytes_posted: posts * 100,
                posts_high: 0, posts_broadcast: 0, pruned: 0, skipped: 0, snapped: 0 },
            latency_buckets_ms: vec![0, 10], latency_histogram_ms: vec![0, deliveries, 0],
            size_buckets_bytes: vec![0, 64], size_histogram_bytes: vec![0, posts, 0],
            active_waiters: 0, active_followers: 0,
            partitions: vec![], publishers: vec![], top_topics: vec![], top_consumers: vec![],
        }
    }

    #[test]
    fn tick_appends_a_minute_bucket_after_enough_seconds() {
        let mut app = App::new();
        for _ in 0..59 {
            app = app.tick(report(1, 1, 1, 1), 1);
        }
        assert_eq!(app.series().minute_buckets.len(), 0);
        app = app.tick(report(60, 60, 60, 1), 1); // 60th second
        assert_eq!(app.series().minute_buckets.len(), 1);
    }

    #[test]
    fn a_restart_emits_a_rebaseline_marker_not_a_negative_spike() {
        let mut app = App::new();
        app = app.tick(report(10, 8, 100, 1), 1);
        // Daemon restarts: uptime shrinks, pid changes. No diff, but the
        // series is preserved and a marker is set.
        app = app.tick(report(0, 0, 1, 2), 1);
        assert!(app.last_restart_ms().is_some(), "a restart must tag the app");
        // The minute bucket's posts must not be negative from a fresh zero diff.
        if let Some(bucket) = app.series().minute_buckets.last() {
            assert!(bucket.posts < 1000, "no synthetic spike from rebaseline");
        }
    }

    #[test]
    fn rolling_minute_buckets_capped_at_sixty() {
        let mut app = App::new();
        for minute in 0..120u64 {
            for _ in 0..60 {
                app = app.tick(report(minute, minute, minute * 60 + 60, 1), 1);
            }
        }
        assert!(app.series().minute_buckets.len() <= 60);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p agent-bus-cli app 2>&1 | tail -10`
Expected: `cannot find type App`.

- [ ] **Step 3: Implement `app.rs`**

Prepend to `crates/cli/src/dashboard/app.rs`:

```rust
//! Pure dashboard state. [`App::tick`] takes the current report and the
//! elapsed seconds since the previous tick and returns the next [`App`]. The
//! UI then renders from an `&App`. Keeping both pure lets the whole board be
//! unit-tested without a terminal.

use std::time::{SystemTime, UNIX_EPOCH};

use agent_bus_protocol::MetricsReport;

use super::sample::Sample;

/// One-minute aggregate of per-second diffs. 60 of these form the one-hour view.
#[derive(Debug, Clone, Default)]
pub struct MinuteBucket {
    pub posts: u64,
    pub deliveries: u64,
    pub bytes: u64,
    pub avg_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub avg_size_bytes: u64,
    /// Set when this minute contained a daemon-restart rebaseline.
    pub had_restart: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Series {
    pub minute_buckets: Vec<MinuteBucket>,
    pub cur_posts: u64,
    pub cur_deliveries: u64,
    pub cur_bytes: u64,
    pub cur_latency_samples: u64,
    pub cur_latency_sum_ms: u64,
    pub cur_p95_ms: u64,
    pub cur_size_samples: u64,
    pub cur_size_sum_bytes: u64,
    pub cur_had_restart: bool,
    pub secs_into_minute: u64,
}

impl Series {
    pub fn accumulate(&mut self, posts: u64, deliveries: u64, bytes: u64,
                      lat_samples: u64, lat_sum_ms: u64, p95_ms: u64,
                      size_samples: u64, size_sum_bytes: u64, had_restart: bool) {
        self.cur_posts += posts;
        self.cur_deliveries += deliveries;
        self.cur_bytes += bytes;
        self.cur_latency_samples += lat_samples;
        self.cur_latency_sum_ms += lat_sum_ms;
        self.cur_p95_ms = self.cur_p95_ms.max(p95_ms);
        self.cur_size_samples += size_samples;
        self.cur_size_sum_bytes += size_sum_bytes;
        self.cur_had_restart |= had_restart;
    }

    pub fn flush(&mut self) -> MinuteBucket {
        let b = MinuteBucket {
            posts: self.cur_posts,
            deliveries: self.cur_deliveries,
            bytes: self.cur_bytes,
            avg_latency_ms: if self.cur_latency_samples == 0 { 0 }
                else { self.cur_latency_sum_ms / self.cur_latency_samples },
            p95_latency_ms: self.cur_p95_ms,
            avg_size_bytes: if self.cur_size_samples == 0 { 0 }
                else { self.cur_size_sum_bytes / self.cur_size_samples },
            had_restart: self.cur_had_restart,
        };
        *self = Series::default();
        b
    }
}

#[derive(Debug, Clone, Default)]
pub struct App {
    pub series: Series,
    prev: Option<MetricsReport>,
    last_restart_ms: Option<u64>,
    pub paused: bool,
    pub latest: Option<MetricsReport>,
}

impl App {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    #[must_use]
    pub fn tick(mut self, report: MetricsReport, elapsed_secs: u64) -> Self {
        self.latest = Some(report.clone());
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        let prev = match self.prev.take() {
            Some(p) => p,
            None => { self.prev = Some(report); return self; }
        };

        match Sample::diff(&prev, &report, elapsed_secs) {
            Some(diff) => {
                let lat_samples: u64 = diff.latency_delta.iter().sum();
                let size_samples: u64 = diff.size_delta.iter().sum();
                let lat_sum = diff.avg_latency_ms.saturating_mul(lat_samples);
                self.series.accumulate(
                    diff.posts_per_sec as u64,
                    diff.deliveries_per_sec as u64,
                    diff.bytes_per_sec as u64,
                    lat_samples, lat_sum, diff.p95_latency_ms,
                    size_samples, diff.avg_size_bytes.saturating_mul(size_samples),
                    false,
                );
                self.series.secs_into_minute += elapsed_secs;
                if self.series.secs_into_minute >= 60 {
                    let bucket = self.series.flush();
                    self.series.minute_buckets.push(bucket);
                    if self.series.minute_buckets.len() > 60 {
                        self.series.minute_buckets.remove(0);
                    }
                    self.series.secs_into_minute = 0;
                }
            }
            None => {
                // Daemon restarted: tag the current minute as having a gap.
                self.last_restart_ms = Some(now_ms);
                self.series.cur_had_restart = true;
            }
        }
        self.prev = Some(report);
        self
    }

    #[must_use]
    pub fn series(&self) -> &Series { &self.series }
    #[must_use]
    pub fn last_restart_ms(&self) -> Option<u64> { self.last_restart_ms }
}
```

Update `crates/cli/src/dashboard/mod.rs` to declare `pub mod app;` and `pub mod ui;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p agent-bus-cli app 2>&1 | tail -10`
Expected: `test result: ok. 3 passed; 0 failed`.

- [ ] **Step 5: Implement `ui.rs` (pure render)**

Create `crates/cli/src/dashboard/ui.rs` with the layout described in the design's *Display* section. Six rows in a `Layout::vertical([Length(3), Length(6), Length(7), Length(5), Length(4), Fill(1)])`. Each widget is built from `&App` + `&MetricsReport`:

- Row 1: a `Paragraph` with the header (pid, uptime, socket, restart marker, paused).
- Row 2: a `BarChart` of `series.minute_buckets` posts/min, plus a `Sparkline` of deliveries.
- Row 3: a `BarChart` of the live latency histogram delta annotated with avg/p95/max tiles, plus a `Sparkline` of p95 over the minute buckets.
- Row 4: a `BarChart` of the size histogram delta plus avg/p95 tiles and a `Sparkline` of avg size.
- Row 5: a row of `Gauge` widgets (undelivered total, worst-lag, retention pressure = `oldest_age_secs / 3600`, active waiters, active followers, publishers, consumers).
- Row 6: two `Table`s side by side (partitions left, top-topics + top-consumers right).

Signature:

```rust
pub fn render(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    app: &crate::dashboard::app::App,
    latest: Option<&agent_bus_protocol::MetricsReport>,
    socket: &std::path::Path,
    refresh_ms: u64,
) { /* ... */ }
```

Keep cosmetic styling minimal: default ratatui `Block::bordered()` with a title per panel. Use `Style::default().fg(Color::Yellow)` for the restart marker and `Color::Red` for `paused`. If `area.height < 24 || area.width < 80`, render a single "terminal too small" notice and return.

- [ ] **Step 6: Add the dependencies**

In `crates/cli/Cargo.toml`, add to `[dependencies]`:

```toml
ratatui = "0.29"
crossterm = "0.28"
```

- [ ] **Step 7: Build and lint**

Run: `cargo build -p agent-bus-cli 2>&1 | tail -10`
Expected: compiles. `ui::render` may be unused until Task 7 wires the loop; if clippy warns `dead_code`, either stub the call from `dashboard::run` now or mark `ui::render` `#[allow(dead_code)]` until Task 7 lands.

Run: `cargo clippy -p agent-bus-cli --all-targets 2>&1 | tail -10`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/cli
git commit -m "feat(cli): dashboard App state and ratatui render"
```

---

## Task 7: `dashboard` subcommand wiring, integration test, docs

**Files:**
- Modify: `crates/cli/src/cli.rs`
- Modify: `crates/cli/src/main.rs`
- Modify: `crates/cli/src/dashboard/mod.rs` (add `run`)
- Modify: `crates/cli/tests/integration.rs`
- Modify: `README.md`

- [ ] **Step 1: Add the subcommand parser and tests**

In `crates/cli/src/cli.rs`, add to the `Command` enum:

```rust
    /// Live, full-screen dashboard of bus metrics and throughput.
    Dashboard {
        /// Poll interval, e.g. `1s`, `5s`. Default 1s. Min 1s, max 60s.
        #[arg(long, value_parser = parse_refresh_secs, default_value = "1")]
        refresh: u64,
    },
```

Add the parser:

```rust
/// Parse a refresh duration to seconds, clamped to the dashboard's bounds.
///
/// # Errors
/// Returns an error if the input is not a recognized duration.
pub fn parse_refresh_secs(input: &str) -> Result<u64> {
    let secs = parse_duration_secs(input)?;
    Ok(secs.clamp(1, 60))
}
```

Add tests to the `tests` module:

```rust
    #[test]
    fn parses_and_clamps_refresh() {
        assert_eq!(parse_refresh_secs("5s").unwrap(), 5);
        assert_eq!(parse_refresh_secs("0").unwrap(), 1, "below the floor clamps up");
        assert_eq!(parse_refresh_secs("90s").unwrap(), 60, "above the ceiling clamps down");
    }
```

- [ ] **Step 2: Run the parser test (it fails until the arm exists)**

Run: `cargo test -p agent-bus-cli parses_and_clamps_refresh 2>&1 | tail -10`
Expected: fails to compile until the `Command::Dashboard` variant is added in Step 1's edit; once added, the test passes.

- [ ] **Step 3: Add the `dashboard::run` entry point**

In `crates/cli/src/dashboard/mod.rs`:

```rust
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::thread;

use agent_bus_protocol::{Request, Response};
use anyhow::Result;
use ratatui::{Terminal, backend::CrosstermBackend};
use crossterm::execute;
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode};

use crate::client::Client;
use crate::cli::ExitCode;
use super::app::App;
use super::ui;

pub fn run(refresh_secs: u64) -> Result<ExitCode> {
    let refresh = Duration::from_secs(refresh_secs);
    let socket = crate::client::socket_path();

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel::<Option<(agent_bus_protocol::MetricsReport, u64)>>(8);

    // Poll thread: one blocking Client on its own thread.
    let stop_clone = Arc::clone(&stop);
    thread::Builder::new().name("agent-bus poll".into()).spawn(move || {
        let mut client: Option<Client> = Client::connect().ok();
        let mut prev_at = Instant::now();
        while !stop_clone.load(Ordering::Relaxed) {
            thread::sleep(refresh);
            let elapsed = prev_at.elapsed().as_secs().max(1);
            prev_at = Instant::now();
            let pair = match client.as_mut() {
                Some(c) => match c.request(&Request::Metrics) {
                    Ok(Response::Metrics { metrics }) => Some((metrics, elapsed)),
                    _ => None,
                },
                None => None,
            };
            if pair.is_none() && client.is_none() {
                client = Client::connect().ok();
            }
            let _ = tx.send(pair);
        }
    })?;

    // UI thread (main): raw mode + alt screen, render loop.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let result = render_loop(&mut terminal, &rx, &mut app, &socket, refresh_secs, &stop);

    // Restore terminal no matter what.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result.map(|()| ExitCode::Success)
}

fn render_loop<B>(
    terminal: &mut Terminal<B>,
    rx: &mpsc::Receiver<Option<(agent_bus_protocol::MetricsReport, u64)>>,
    app: &mut App,
    socket: &std::path::Path,
    refresh_secs: u64,
    stop: &Arc<AtomicBool>,
) -> Result<()>
where B: ratatui::backend::Backend
{
    use crossterm::event;
    loop {
        // Drain pending reports without blocking so a slow terminal never
        // builds a backlog.
        while let Ok(pair) = rx.try_recv() {
            if let Some((report, elapsed)) = pair {
                *app = std::mem::take(app).tick(report, elapsed);
            }
        }
        terminal.draw(|f| {
            ui::render(f, f.area(), app, app.latest.as_ref(), socket, refresh_secs * 1000);
        })?;
        if event::poll(Duration::from_millis(250))? {
            if let event::Event::Key(k) = event::read()? {
                match k.code {
                    event::KeyCode::Char('q') => { stop.store(true, Ordering::Relaxed); break; }
                    event::KeyCode::Char('p') => { app.paused = !app.paused; }
                    event::KeyCode::Char('r') => { *app = App::new(); }
                    _ => {}
                }
            }
        }
        if stop.load(Ordering::Relaxed) { break; }
    }
    Ok(())
}
```

If `socket_path` is not already `pub` in `crates/cli/src/client.rs`, expose it (`pub fn socket_path()`) so the dashboard can show it.

- [ ] **Step 4: Wire into `main.rs`**

In `crates/cli/src/main.rs`'s `run`, add among the arms:

```rust
        Command::Dashboard { refresh } => crate::dashboard::run(refresh),
```

- [ ] **Step 5: Add an integration test**

In `crates/cli/tests/integration.rs`, append a test that drives the real
daemon. Mirror the existing `post_before_wait_is_delivered` test for the
harness's `request`/`Client` shape:

```rust
#[test]
fn metrics_reports_activity_via_a_real_daemon() {
    let state = TestState::unique();
    state.connect(); // boots the daemon
    state.request(&Request::Ensure { pattern: "iot_base/**".into() });

    for body in ["one", "two", "three"] {
        state.request(&Request::Post {
            topic: "iot_base/dev_01".into(), body: body.into(),
            priority: Priority::Normal, from: Some("dev_01".into()), broadcast: false,
        });
    }
    // Deliver one so deliveries > 0.
    let _ = state.request(&Request::Wait {
        pattern: "iot_base/**".into(), label: "rev".into(), timeout_secs: Some(5),
    });

    match state.request(&Request::Metrics) {
        Response::Metrics { metrics } => {
            assert_eq!(metrics.totals.posts, 3);
            assert!(metrics.totals.deliveries >= 1);
            assert!(metrics.latency_histogram_ms.iter().sum::<u64>() >= 1);
            assert_eq!(metrics.publishers, vec!["dev_01".to_owned()]);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}
```

- [ ] **Step 6: Run the full suite**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all pass, including the new `metrics_reports_activity_via_a_real_daemon`.

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -10`
Expected: no warnings.

Run: `cargo fmt --all -- --check`
Expected: clean.

- [ ] **Step 7: Update the README**

Add one row to the commands table:

```
| `dashboard` | Live metrics: throughput, latency, lag, messages |
```

And add a short section below the commands table:

```markdown
## Dashboard

`agent-bus dashboard` opens a full-screen, live view of the bus: posts per
minute, post-to-delivery latency (avg / p95 / max), message-size distribution,
backlog gauges, active waiters and followers, and per-partition tables. Leave
it running in a corner terminal. `q` quits, `p` pauses, `r` resets the series,
`+`/`-` adjusts the refresh interval. The dashboard is the long-lived observer;
the daemon can restart underneath it and the dashboard rebaselines and continues.
```

- [ ] **Step 8: Commit**

```bash
git add README.md crates/cli
git commit -m "feat(cli): live dashboard subcommand with integration coverage"
```

---

## Verification

After all tasks:

```bash
cargo test --workspace                    # all pass, including new dashboard tests
cargo clippy --workspace --all-targets    # pedantic, warning-free
cargo fmt --all -- --check
cargo install --path crates/cli           # `agent-bus dashboard` runs
```

Manual smoke test:

```bash
agent-bus daemon 'iot_base/**'
agent-bus post iot_base/dev_01 "hello"
agent-bus dashboard            # in another terminal; should show 1 post, latency tick
```