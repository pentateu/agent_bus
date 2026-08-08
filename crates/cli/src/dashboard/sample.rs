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
#[allow(dead_code)] // wired into `app`/`ui` in Task 6; remove the allow with it.
pub const LATENCY_BUCKETS_MS: &[u64] =
    &[0, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000, 60000];

#[must_use]
#[allow(dead_code)] // wired into `app` in Task 6; remove the allow with it.
pub fn bin_index(boundaries: &[u64], value: u64) -> usize {
    boundaries.iter().filter(|&&b| b <= value).count()
}

/// One poll's worth of derived numbers, suitable for appending to a series.
#[allow(dead_code)] // wired into `app` in Task 6; remove the allow with it.
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

/// Counts per second, as an `f64`.
///
/// Casts are fine here: the numerator and denominator are small counters and
/// elapsed seconds, and a rate is only ever rendered or compared loosely.
#[allow(clippy::cast_precision_loss, dead_code)] // dead until `app` lands in Task 6.
fn per_second(delta: u64, secs: u64) -> f64 {
    delta as f64 / secs.max(1) as f64
}

/// Diff two reports. Returns `None` if the daemon restarted between them
/// (uptime shrank or pid changed), in which case the caller drops the diff
/// and tags a rebaseline.
#[allow(dead_code)] // wired into `app` in Task 6; remove the allow with it.
pub struct Sample;
impl Sample {
    #[must_use]
    #[allow(dead_code)] // dead until `app` lands in Task 6.
    pub fn diff(prev: &MetricsReport, cur: &MetricsReport, elapsed_secs: u64) -> Option<Diff> {
        if cur.uptime_secs < prev.uptime_secs || cur.pid != prev.pid {
            return None;
        }

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
        // `checked_div`: 0 samples means this interval saw no delivery, and a
        // divide by zero would silently produce a bogus 0ms average.
        let avg_latency_ms = weighted_mid(&latency_delta, &cur.latency_buckets_ms)
            .checked_div(total_lat_samples)
            .unwrap_or(0);
        let p95_latency_ms = if total_lat_samples == 0 {
            0
        } else {
            let bucket = percentile_bucket(&latency_delta, 0.95);
            // The catch-all bin has no boundary of its own: lower-bound the p95
            // at the last boundary instead of reporting 0 for ">= 60s".
            cur.latency_buckets_ms
                .get(bucket)
                .copied()
                .unwrap_or_else(|| cur.latency_buckets_ms.last().copied().unwrap_or(0))
        };

        let total_size_samples: u64 = size_delta.iter().sum();
        let avg_size_bytes = weighted_mid(&size_delta, &cur.size_buckets_bytes)
            .checked_div(total_size_samples)
            .unwrap_or(0);

        Some(Diff {
            posts_per_sec: per_second(
                cur.totals.posts.saturating_sub(prev.totals.posts),
                elapsed_secs,
            ),
            deliveries_per_sec: per_second(
                cur.totals.deliveries.saturating_sub(prev.totals.deliveries),
                elapsed_secs,
            ),
            bytes_per_sec: per_second(
                cur.totals.bytes_posted.saturating_sub(prev.totals.bytes_posted),
                elapsed_secs,
            ),
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
///
/// The casts are deliberate: `q` is a literal like `0.95` and totals here are
/// small poll deltas, far below `2^53`, so neither precision nor sign can
/// actually be lost.
#[allow(
    dead_code, // wired into `app` in Task 6; remove the allow with it.
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
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
#[allow(dead_code)] // wired into `app` in Task 6; remove the allow with it.
fn weighted_mid(histogram: &[u64], boundaries: &[u64]) -> u64 {
    let mut sum: u64 = 0;
    for (i, &count) in histogram.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let lo = boundaries.get(i).copied().unwrap_or(0);
        let hi = boundaries.get(i + 1).copied().unwrap_or(lo * 2 + 1);
        let mid = lo + hi.saturating_sub(lo) / 2;
        sum = sum.saturating_add(mid.saturating_mul(count));
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bus_protocol::{MetricsReport, MetricsTotals};

    fn report(posts: u64, deliveries: u64, uptime: u64, pid: u32) -> MetricsReport {
        MetricsReport {
            pid,
            uptime_secs: uptime,
            poll_at_ms: 0,
            totals: MetricsTotals {
                posts,
                deliveries,
                bytes_posted: 0,
                posts_high: 0,
                posts_broadcast: 0,
                pruned: 0,
                skipped: 0,
                snapped: 0,
            },
            latency_buckets_ms: vec![0, 10],
            latency_histogram_ms: vec![0, 0, 0],
            size_buckets_bytes: vec![0, 64],
            size_histogram_bytes: vec![0, 0, 0],
            active_waiters: 0,
            active_followers: 0,
            partitions: vec![],
            publishers: vec![],
            top_topics: vec![],
            top_consumers: vec![],
        }
    }

    /// `float_cmp` pedantic forbids `==` on f64; rates are derived, so compare
    /// with a tolerance that survives the division rounding.
    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    #[test]
    fn diff_yields_per_second_rate() {
        let prev = report(10, 8, 100, 1);
        let cur = report(13, 9, 101, 1);
        let d = Sample::diff(&prev, &cur, 1).unwrap();
        assert_close(d.posts_per_sec, 3.0);
        assert_close(d.deliveries_per_sec, 1.0);
    }

    #[test]
    fn diff_over_a_longer_interval_divides_by_elapsed_seconds() {
        let prev = report(10, 8, 100, 1);
        let cur = report(40, 18, 110, 1); // 10s elapsed
        let d = Sample::diff(&prev, &cur, 10).unwrap();
        assert_close(d.posts_per_sec, 3.0);
        assert_close(d.deliveries_per_sec, 1.0);
    }

    #[test]
    fn a_daemon_restart_returns_none() {
        let prev = report(10, 8, 500, 1);
        let cur = report(0, 0, 2, 2); // fresh daemon, pid changed, uptime shrank
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
