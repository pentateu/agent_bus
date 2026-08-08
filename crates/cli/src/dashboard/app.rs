// The tick/flush surface is only reachable once the `dashboard` subcommand
// (Task 7) drives it; the allow goes with that wiring.
#![allow(dead_code)]

//! App state for the dashboard's rolling one-hour view.
//!
//! `App` is pure state: every poll produces a new `App` via [`App::tick`].
//! The socket thread is a separate, dumb poller; it only sends events. Ticks
//! are "now minus the previous tick" seconds — the poll interval (~1s).
//!
//! Series semantics:
//! * one `MinuteBucket` per minute; the current minute's accumulators live in
//!   the [`Series`] until [`MINUTE_SECS`] elapse, then they are flushed.
//! * on daemon restart the diff contributes nothing; the app zeroes the
//!   current minute's accumulators (the whole hour window is rebased onto the
//!   fresh daemon), keeps the flushed history, and sets
//!   [`App::last_restart_ms`] so the UI can show telemetry was lost.

use super::sample::Sample;
use agent_bus_protocol::MetricsReport;

/// Seconds per minute bucket (60, by design).
pub const MINUTE_SECS: u64 = 60;

/// Cap on the rolling window: 60 minute buckets = 1 hour.
pub const MAX_MINUTES: usize = 60;

/// Aggregates of one finished minute. Filled by [`Series::flush_bucket`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MinuteBucket {
    pub posts: u64,
    pub deliveries: u64,
    pub bytes: u64,
    pub sample_count: u64,
    pub latency_sum_ms: u64,
    pub p95_ms: u64,
    pub size_sum: u64,
}

/// The rolling one-hour series plus the current minute's accumulators.
#[derive(Debug, Clone, Default)]
pub struct Series {
    pub minute_buckets: Vec<MinuteBucket>,
    pub cur_posts: u64,
    pub cur_deliveries: u64,
    pub cur_bytes: u64,
    pub cur_latency_sum_ms: u64,
    pub cur_latency_samples: u64,
    pub cur_p95_ms: u64,
    pub cur_size_sum: u64,
    pub cur_size_samples: u64,
    pub secs_into_minute: u64,
}

impl Series {
    /// Move the current minute's accumulators into a finished bucket and
    /// append it to the rolling window, dropping the oldest past the cap.
    fn flush_bucket(&mut self) {
        if self.secs_into_minute == 0 {
            return;
        }
        let bucket = MinuteBucket {
            posts: self.cur_posts,
            deliveries: self.cur_deliveries,
            bytes: self.cur_bytes,
            sample_count: self.cur_latency_samples.max(self.cur_size_samples),
            latency_sum_ms: self.cur_latency_sum_ms,
            p95_ms: self.cur_p95_ms,
            size_sum: self.cur_size_sum,
        };
        self.minute_buckets.push(bucket);
        if self.minute_buckets.len() > MAX_MINUTES {
            self.minute_buckets.remove(0);
        }
        self.cur_posts = 0;
        self.cur_deliveries = 0;
        self.cur_bytes = 0;
        self.cur_latency_sum_ms = 0;
        self.cur_latency_samples = 0;
        self.cur_p95_ms = 0;
        self.cur_size_sum = 0;
        self.cur_size_samples = 0;
        self.secs_into_minute = 0;
    }

    /// Average delivery latency so far this minute, in ms.
    #[must_use]
    #[allow(dead_code)] // wired into `ui` in Task 6; remove the allow with it.
    pub fn avg_latency_ms(&self) -> u64 {
        self.cur_latency_sum_ms.checked_div(self.cur_latency_samples.max(1)).unwrap_or(0)
    }

    /// Average posted message size so far this minute, in bytes.
    #[must_use]
    #[allow(dead_code)] // wired into `ui` in Task 6; remove the allow with it.
    pub fn avg_size_bytes(&self) -> u64 {
        self.cur_size_sum.checked_div(self.cur_size_samples.max(1)).unwrap_or(0)
    }
}

/// The dashboard's pure state.
#[derive(Debug, Clone, Default)]
pub struct App {
    series: Series,
    /// The report the next poll will be diffed against. After `tick` this is
    /// also "the latest report seen", read by the live gauges and tables.
    prev: Option<MetricsReport>,
    /// Set when the previous poll saw a daemon restart (pid/uptime change).
    had_restart: bool,
    /// `poll_at_ms` of the first poll after a restart; 0 until one happens.
    last_restart_ms: u64,
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn series(&self) -> &Series {
        &self.series
    }

    /// The most recent report — feeds the live gauges and tables.
    #[must_use]
    pub fn last_report(&self) -> Option<&MetricsReport> {
        self.prev.as_ref()
    }

    #[must_use]
    #[allow(dead_code)] // wired into `ui` in Task 6; remove the allow with it.
    pub fn had_restart(&self) -> bool {
        self.had_restart
    }

    /// `poll_at_ms` of the poll that detected a restart — for the header's
    /// "telemetry reset" note. `None` until a restart has been seen.
    #[must_use]
    pub fn last_restart_ms(&self) -> Option<u64> {
        self.had_restart.then_some(self.last_restart_ms)
    }

    /// Advance the app by one poll: diff against the previous report, fold
    /// into the current minute's accumulators, flush a bucket at the minute
    /// boundary. A restart (diff `None`) rebaselines without a spike.
    pub fn tick(&mut self, report: &MetricsReport, elapsed_secs: u64) {
        if let Some(prev) = &self.prev {
            if let Some(diff) = Sample::diff(prev, report, elapsed_secs) {
                self.series.cur_posts = self.series.cur_posts.saturating_add(diff.posts_delta);
                self.series.cur_deliveries =
                    self.series.cur_deliveries.saturating_add(diff.deliveries_delta);
                self.series.cur_bytes = self.series.cur_bytes.saturating_add(diff.bytes_delta);
                let latency_samples: u64 = diff.latency_delta.iter().sum();
                self.series.cur_latency_samples =
                    self.series.cur_latency_samples.saturating_add(latency_samples);
                self.series.cur_latency_sum_ms =
                    self.series.cur_latency_sum_ms.saturating_add(diff.latency_sum_ms);
                self.series.cur_p95_ms = diff.p95_latency_ms.max(self.series.cur_p95_ms);
                let size_samples: u64 = diff.size_delta.iter().sum();
                self.series.cur_size_samples =
                    self.series.cur_size_samples.saturating_add(size_samples);
                self.series.cur_size_sum =
                    self.series.cur_size_sum.saturating_add(diff.size_sum_bytes);
                self.series.secs_into_minute =
                    self.series.secs_into_minute.saturating_add(elapsed_secs);
                if self.series.secs_into_minute >= MINUTE_SECS {
                    self.series.flush_bucket();
                }
            } else {
                self.had_restart = true;
                self.last_restart_ms = report.poll_at_ms;
                self.rebase_current_minute();
            }
        }
        self.prev = Some(report.clone());
    }

    /// Zero the current minute's accumulators after a restart: the window is
    /// rebased onto the fresh daemon, so the unfinished minute must not
    /// average a truncated history into its next tick.
    fn rebase_current_minute(&mut self) {
        self.series.cur_posts = 0;
        self.series.cur_deliveries = 0;
        self.series.cur_bytes = 0;
        self.series.cur_latency_sum_ms = 0;
        self.series.cur_latency_samples = 0;
        self.series.cur_p95_ms = 0;
        self.series.cur_size_sum = 0;
        self.series.cur_size_samples = 0;
        self.series.secs_into_minute = 0;
    }
}

#[cfg(test)]
pub(crate) mod support {
    use super::*;
    use agent_bus_protocol::MetricsTotals;

    /// A minimal report whose totals/histograms reflect `posts` and
    /// `deliveries` so diffs and buckets have something to chew on.
    pub fn report(posts: u64, deliveries: u64, uptime: u64, pid: u32) -> MetricsReport {
        MetricsReport {
            pid,
            uptime_secs: uptime,
            poll_at_ms: 0,
            totals: MetricsTotals {
                posts,
                deliveries,
                bytes_posted: posts * 100,
                posts_high: 0,
                posts_broadcast: 0,
                pruned: 0,
                skipped: 0,
                snapped: 0,
            },
            latency_buckets_ms: vec![0, 10],
            latency_histogram_ms: vec![0, deliveries, 0],
            size_buckets_bytes: vec![0, 64],
            size_histogram_bytes: vec![0, posts, 0],
            active_waiters: 0,
            active_followers: 0,
            partitions: vec![],
            publishers: vec![],
            top_topics: vec![],
            top_consumers: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::support::report;
    use super::{App, MAX_MINUTES};

    #[test]
    fn tick_appends_a_minute_bucket_after_enough_seconds() {
        let mut app = App::new();
        for _ in 0..60 {
            app.tick(&report(1, 1, 1, 1), 1);
        }
        assert_eq!(app.series().minute_buckets.len(), 0);
        app.tick(&report(60, 60, 60, 1), 1); // 60th second
        assert_eq!(app.series().minute_buckets.len(), 1);
    }

    #[test]
    fn a_restart_emits_a_rebaseline_marker_not_a_negative_spike() {
        let mut app = App::new();
        app.tick(&report(10, 8, 100, 1), 1);
        // Daemon restarts: uptime shrinks, pid changes. No diff, but the app
        // rebases and tags the restart for the header.
        app.tick(&report(0, 0, 1, 2), 1);
        assert!(app.last_restart_ms().is_some(), "a restart must tag the app");
        // The current minute's counters are zeroed, not negative.
        assert_eq!(app.series().cur_posts, 0);
    }

    #[test]
    fn rolling_minute_buckets_capped_at_sixty() {
        let mut app = App::new();
        for minute in 0..120u64 {
            for _ in 0..60 {
                app.tick(&report(minute, minute, minute * 60 + 60, 1), 1);
            }
        }
        assert!(app.series().minute_buckets.len() <= MAX_MINUTES);
    }
}
