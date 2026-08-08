# agent-bus Dashboard — Design

**Date:** 2026-08-07
**Status:** Proposed
**Depends on:** [`2026-08-06-agent-bus-design.md`](2026-08-06-agent-bus-design.md)

## Purpose

A live, full-screen terminal dashboard — `agent-bus dashboard` — that shows
throughput, delivery latency, message sizes, backlog, and participant activity
across every partition the daemon owns. The intended use is one long-lived
terminal left open in a corner that the operator checks from time to time,
exactly like `htop` or `kubectl watch`.

The bus is a coordination tool for agents that fire in bursts and then go
quiet. Counts alone (`agent-bus status`) show the *static* picture — what is
sitting in the log right now. The dashboard shows the *dynamic* picture — how
fast messages are moving, how long they wait before someone picks them up, and
whether any consumer is falling behind.

## Non-goals

- **Persisted metrics.** The daemon does not write metrics to disk. Either side
  of the pipe can be restarted independently; the dashboard rebaselines when it
  notices a daemon restart (uptime went backwards) and keeps displaying. This
  matches the existing architecture: only the durable log and cursors are on
  disk, everything else is in-memory and rebuilt on start.
- **Historical analytics past one hour.** The retention window is one hour, so
  any series longer than that is fabricated. The dashboard's longest view is
  60 one-minute buckets.
- **Alerting, thresholds, web UI, remote scraping.** Local-only, one operator,
  one terminal.

## Core model

**Two stores, one report.** The daemon accumulates *cumulative counters and
histograms since its own start*; the dashboard accumulates a *rolling time
series since the dashboard's own start*. The wire between them carries a single
`MetricsReport` snapshot. This split keeps the daemon dumb and the display
state where the operator can see it — in the terminal they left open.

**Why cumulative on the daemon, deltas on the dashboard.** A cumulative counter
is trivial to merge with the previous snapshot: `rate = cur - prev`. It is also
restart-safe: a daemon that exits and comes back reports fresh zeros, the
dashboard notices `uptime_secs` went backwards, drops one diff, and continues.
A bucketed time series on the daemon would either be lost on restart or have to
be persisted — both worse.

**Why histograms, not raw samples.** Latency and message-size distributions are
what an operator reads off a graph, but keeping every sample is unbounded. A
fixed set of buckets (see *Metrics schema*) captures the shape of both
distributions in constant space, and the dashboard reconstructs avg / p50 / p95
/ max from them. The dashboard also keeps the *delta* of the histogram each
poll, so "p95 in the last minute" is p95 of that minute's delta, not of all
time.

## Architecture

One new subcommand, three touched crates, one new wire type.

- **`agent-bus-daemon`** gains a `BusMetrics` struct held inside `BusState` and a
  `PartitionMetrics` held inside each `Partition`. Instrumentation is wired into
  the existing `publish`, `deliver`, and `prune` calls — the same code paths the
  daemon already runs under its single state lock, so no new locking.
  `Request::Metrics` is dispatched in `handler.rs` exactly the way
  `Request::Status` is.
- **`agent-bus-protocol`** gains `Request::Metrics`, `Response::Metrics`, and
  the `MetricsReport` / `PartitionMetrics` / `TopicCount` types. Kept in the
  shared crate so the daemon and CLI cannot drift, the same reason `StatusReport`
  lives there.
- **`agent-bus-cli`** gains a `dashboard` module and a `dashboard` subcommand.
  The dashboard is a long-lived client that polls `Request::Metrics` (and
  `Request::Status` for the partition table) once a second, maintains a rolling
  series in its own memory, and renders with `ratatui` + `crossterm`. None of
  the existing short-lived commands change.

**Active waiters and followers.** Connections parked in `wait_for_message` or
`follow` (`server.rs`) are not held inside the state mutex, so two `Arc<AtomicU64>`
counters — `active_waiters`, `active_followers` — are created in `serve()` and
passed to the connection tasks. They bump on entry to the parked loop and drop
on exit (including all error paths), so a connection that dies still decrements.
`build_metrics` reads them.

**Transport.** Unchanged: the dashboard is an ordinary Unix-socket client. It
opens one connection and sends one `Request::Metrics` (plus one
`Request::Status`) per poll; the daemon's connection loop already handles
multiple requests per connection, so no server change is needed for the polling
itself.

## Metrics schema

```json
{
  "pid": 48213,
  "uptime_secs": 942,
  "poll_at_ms": 1700000000000,
  "totals": {
    "posts": 137,
    "deliveries": 129,
    "bytes_posted": 28431,
    "posts_high": 4,
    "posts_broadcast": 22,
    "pruned": 18,
    "skipped": 0,
    "snapped": 0
  },
  "latency_buckets_ms":     [0, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000, 60000],
  "latency_histogram_ms":   [3, 41, 60, 18, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  "size_buckets_bytes":     [0, 32, 64, 128, 256, 512, 1024, 4096, 16384, 65536, 262144, 1048576, 4194304],
  "size_histogram_bytes":  [0, 8, 30, 55, 30, 10, 4, 0, 0, 0, 0, 0, 0, 0],
  "active_waiters": 1,
  "active_followers": 0,
  "partitions": [
    {
      "name": "iot_base",
      "message_count": 84,
      "oldest_age_secs": 123,
      "undelivered_lag": 8,
      "participants": 3,
      "posts": 137, "deliveries": 129, "bytes": 28431,
      "pruned": 18, "skipped": 0, "snapped": 0
    }
  ],
  "publishers": ["dev_01", "reviewer_01", "planner"],
  "top_topics": [
    { "topic": "iot_base/dev_01", "count": 60, "total_bytes": 12000 }
  ],
  "top_consumers": [
    { "label": "reviewer_01", "partition": "iot_base", "pattern": "iot_base/**",
      "broadcast": false, "lag": 5 }
  ]
}
```

**Buckets are fixed in the wire format.** `latency_buckets_ms` and
`size_buckets_bytes` are sent on every response — they are constant, but
sending them means the dashboard never hardcodes a boundary and a future
daemon with different buckets cannot desync a client. There are always one more
histogram cells than boundaries: the final cell is "≥ the last boundary".

**Latency** is "post-to-first-delivery": `delivery_now_ms - msg.id.timestamp_ms()`.
For exclusive messages it is recorded once (at the single delivery); for
broadcast messages it is recorded per consumer label that receives it, because
"how long until *each* agent picked it up" is the question the operator asks.
`now_ms` comes from the same `SystemTime` clock the daemon already reads in
`handler::now_secs`, just unpacked to milliseconds.

**`publishers`** is the set of distinct `from` values seen across all retained
logs. Bounded by retention: at one hour of traffic it is a handful of agent
names, not an unbounded set. Computed by walking each partition's in-memory log
at report time, the same way `pattern_snapshots` already walks the log.

**`top_topics`** counts messages per concrete topic in the retained window,
returns the top N (10), with `total_bytes` for a bar chart. Computed alongside
`publishers` from the in-memory log.

**`top_consumers`** is reprojected from each partition's `pattern_snapshots`:
flattened, sorted by `lag` descending, top 10. Carried in `MetricsReport`
rather than derived on the client so the dashboard does not need a second
round trip for sorting.

## Command surface

```
agent-bus dashboard                      live full-screen TUI, 1s refresh
agent-bus dashboard --refresh 5s          adjust poll interval
agent-bus dashboard --partition iot_base  filter the series to one partition
```

New global flag is not added; `--json` does not apply (the dashboard is a TUI).
The dashboard takes its own flags only. `--refresh` accepts the same duration
suffixes as `wait`'s `--timeout`; minimum 1s, maximum 60s, default 1s. A
non-existent or unreachable daemon is not fatal: the dashboard shows
"daemon unavailable — retrying" in the header and keeps polling, because the
whole point is to survive the daemon cycling.

## Display

Full-screen alternate-screen TUI. The board is a fixed six-row grid so the eye
learns where everything lives; nothing reflows. Top to bottom:

1. **Header.** `agent-bus` title, daemon pid, uptime, "daemon restarted
   <sec> ago" marker when a rebaseline happened this session, socket path,
   poll interval, `paused` indicator when the operator pressed `p`.
2. **Throughput.** Bar chart of posts per minute over the last 60 minutes, with
   deliveries/min as a lighter overlaid bar. The headline "how often messages
   are posted" graph.
3. **Latency — the headline "time to pick up" graph.** Bar chart of the live
   latency histogram (current distribution, this minute's delta), plus three
   stat tiles — avg / p95 / max — and a one-line sparkline of p95 over the last
   hour. The operator reads "is the bus slow" off this row.
4. **Message size.** Bar chart of the size histogram, avg / p95 tiles, and a
   sparkline of avg size over the last hour.
5. **Gauges.** Total undelivered (sum of lag), worst-lag consumer name + lag,
   retention pressure (oldest age / 3600 as a 0–100% gauge), active waiters,
   active followers, distinct publishers, distinct consumers. Gauges, not
   numbers, so a glance is enough.
6. **Tables row.** Left: partitions (name, msgs, participants, posts, deliveries,
   oldest, skipped, snapped). Right: top topics (count, bytes) above top
   consumers (lag). Both scrollable with arrow keys.

**Footer / controls.** `q` quit, `p` pause, `r` reset series, `+`/`-` adjust
refresh, `?` help overlay, arrow keys scroll tables. No mouse.

**Rebaseline.** When a poll sees `uptime_secs` decrease or the pid change, the
dashboard clears its `prev_*` snapshots, drops the diff for that one poll (so
the restarted daemon's fresh zeros do not register as a giant negative spike),
tags the current time as `last_restart`, and shows the marker in the header.
The rolling series is preserved across the gap; only that one bucket is
annotated.

## Error handling

- **Daemon temporarily unreachable.** The dashboard keeps polling. The header
  shows "daemon unavailable — retrying in <interval>s" and the series freeze
  where they were. A gap of a few seconds is *annotated* on the sparkline, not
  papered over: a small tick mark says "no data here".
- **Malformed `MetricsReport`.** Treated like a dropped poll: log to stderr (the
  operator may have a newer dashboard than daemon or vice versa) and continue.
  Never panic the TUI; the terminal must always be restored on exit.
- **Terminal too small.** Ratatui's layout collapses gracefully; the dashboard
  shows a "terminal too small: <w>x<h> (need 80x24)" notice rather than
  garbage. The threshold is documented, not guessed.
- **Histogram bucket overflow.** The final bucket is "and above"; it never
  overflows because it is a catch-all. No `u64` saturating arithmetic is
  needed in the daemon beyond the existing `saturating_sub` on age.

## Testing

**Core (unit):** `BusMetrics::record_post` increments the right counters and
the right size bin; `record_delivery` increments the right latency bin from a
fixed `(post_ms, now_ms)` pair; `record_prune` bumps both `pruned` and
`snapped`; `snap_forward` moving a cursor increments the cumulative `snapped`
count (the existing in-memory `snapped` vec and the new counter agree).
Boundary conditions: a latency of exactly 10ms lands in the `≥10` bin, not the
`≥0` bin; a size of exactly 1024 bytes lands in the `≥1024` bin.

**Protocol (unit):** `MetricsReport` round-trips through `encode`/`decode`;
`Request::Metrics` and `Response::Metrics` round-trip; the histograms are
sent as arrays of the documented length.

**Dashboard sample (unit):** Given two synthetic `MetricsReport`s one second
apart, `Sample::diff` produces the right per-second rate and the right
per-second histogram delta. A second report whose `uptime_secs` is *smaller*
than the first triggers a rebaseline (diff is `None`, `last_restart` is set)
and produces no negative spike. Percentile reconstruction from a known
histogram matches a brute-force computation on the same samples.

**Integration:** the existing `tests/integration.rs` harness starts the real
daemon; a new test posts three messages, waits on one (so it is delivered),
then sends `Request::Metrics` over the same client connection and asserts
`totals.posts == 3`, `totals.deliveries >= 1`, and that the `latency_histogram`
is non-empty. A second test posts one message and observes
`active_waiters == 1` while a `wait` is parked (using `tokio::spawn`), then
`== 0` after it resolves.

**UI:** `App::tick` (pure: takes a `MetricsReport`, returns the next `App`
state) and `ui::render` (pure: takes an `App`, draws into a `Buffer`) are
unit-tested with synthetic reports. The ratatui event loop and terminal setup
are the only pieces not covered, and they are thin enough to read.

## Rust practices

Unchanged from the workspace contract: edition 2024, `unsafe_code` forbidden,
`clippy::pedantic` clean, `thiserror` only in `core`, `anyhow` at the binary
boundary, `tokio` in the daemon only. The CLI's tight sync loop stays
synchronous; `ratatui` and `crossterm` are added to `crates/cli`'s manifest only.
The dashboard polls by issuing blocking `Client::request` calls on a std
thread and crossing to the UI thread through a channel, so no async runtime is
introduced into the CLI.