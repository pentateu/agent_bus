# agent-bus — Design

**Date:** 2026-08-06
**Status:** Approved

## Purpose

A lightweight CLI event bus for coordinating AI coding agents that run in
separate, human-initiated terminal sessions. A dev agent posts "ready for
review"; a reviewer agent in another terminal picks it up and starts working.
No shared process, no setup, no broker to install.

Non-goals: network transport, multi-user, delivery guarantees beyond a local
durable log, message schema enforcement.

## Core model

**Partitions.** The first segment of a topic is the partition
(`iot_base/dev_01` → `iot_base`). Partitions are fully isolated: separate log,
separate cursors, separate retention clock. This is what keeps multiple
concurrent projects — each with its own dev/reviewer/planner agents on the same
machine — from crossing streams.

**Topics.** Slash-delimited paths within a partition. Example topics:
`iot_base/dev_01`, `iot_base/reviewer_01`, `iot_base/lessons_learned`,
`iot_base/bugs_found`.

**Wildcards (glob-style).**
- `*` matches exactly one segment: `iot_base/*` matches `iot_base/dev_01`, not
  `iot_base/team/dev_01`.
- `**` matches one or more segments: `iot_base/**` matches any depth.
- A bare partition name is shorthand for `<partition>/**`.

Matching is a pure function over pre-parsed patterns. No string prefixing.

**Cursors.** Each subscriber is identified by an explicit `--as <id>`,
defaulting to the pattern string. The daemon keeps a cursor (last consumed
message id) per `(partition, subscriber-id)`. Cursors survive daemon restart,
which is what lets a restarted agent resume rather than replay or miss.

**Durability.** One append-only JSONL file per partition. The log is the source
of truth; the daemon's in-memory index is rebuilt from it on start.

**Retention.** Messages older than **1 hour** are pruned by a sweep every 60s.
Default replay window for `history` is **30 minutes**.

**Daemon lifetime.** One daemon per OS user, auto-started by whichever command
needs it first. Exits after **1.5 hours** with all partitions idle — deliberately
above the retention window, so by the time it exits the logs hold nothing of
value. Next command brings it back.

## Architecture

Cargo workspace, three crates:

- **`agent-bus-core`** — topic parsing/matching, log format, retention policy,
  cursor arithmetic. No I/O, no async. Pure and unit-testable.
- **`agent-bus-daemon`** — Unix socket server, partition state, subscriber
  registry, retention sweep, idle shutdown.
- **`agent-bus-cli`** — short-lived client for every command; auto-start logic.

**Transport.** Unix domain socket. `$XDG_RUNTIME_DIR/agent-bus.sock`, falling
back to `~/.local/state/agent-bus/agent-bus.sock` on macOS. Newline-delimited
JSON request/response frames.

**State.** `~/.local/state/agent-bus/<partition>.jsonl` plus a sibling
`<partition>.cursors.json`.

**Auto-start race.** Multiple agents across multiple IDEs will fire commands
simultaneously. Start is guarded by an exclusive lockfile plus atomic socket
bind; losers of the race connect to the winner's socket rather than failing.

## Message schema

```json
{
  "id": "01JQ8F2K9X3M4N5P6Q7R8S9T0V",
  "ts": "2026-08-06T14:23:01.482Z",
  "topic": "iot_base/dev_01",
  "priority": "normal",
  "from": "dev_01",
  "body": "completed dev of feature XYZ, commit abc123, ready for review"
}
```

`id` is a ULID: monotonic, lexicographically sortable, and doubles as the
cursor value — so resuming is a binary search into the log. `priority` is
`normal` | `high` and only affects the instruction text hook adapters render.
`body` is opaque; the bus never parses it.

## Command surface

```
agent-bus daemon <pattern>               ensure daemon + partition; print status
agent-bus post <topic> [message]         publish; reads stdin if message omitted
agent-bus wait <pattern> --as <id>       block for next unread; print one; exit
agent-bus read <pattern> --as <id>       drain all unread now, non-blocking
agent-bus follow <pattern> --as <id>     stream forever, one JSON line per message
agent-bus history <pattern> --since 30m  replay window, ignores cursor
agent-bus hook install <harness>         write Stop-hook / plugin config
agent-bus status                         daemon state, partitions, subscribers, lag
agent-bus stop                           shut down
agent-bus guide                          agent-facing usage guide
```

Original flag spellings (`--daemon`, `--subscribe`, `--post`) are retained as
hidden aliases so existing prompt text keeps working.

Global flags: `--json` (all commands), `--timeout <dur>` (`wait`),
`--priority high|normal` (`post`), `--as <id>` (subscriber commands).

**Exit codes** — the caller is a shell loop or an agent branching on the result:

| Code | Meaning |
|------|---------|
| 0 | Success / message delivered |
| 1 | Usage error |
| 2 | `wait` timed out with nothing delivered |
| 3 | Daemon unreachable and could not be started |

Code 2 makes `while agent-bus wait ...; do handle; done` correct by construction.

## Delivery

Two mechanisms, mutually exclusive per agent-moment. Neither can interrupt an
agent mid-tool-call — both harnesses are turn-based, so the honest guarantee is
**delivery at the next turn boundary**.

**1. Blocking `wait`** — the dedicated-waiter case ("wait until dev is done,
then review"). Costs zero tokens while blocked: one tool call in, one result
out, no polling. On connect the daemon checks the log *before* blocking, so a
`wait` that starts after the message was posted returns immediately. This is the
race that makes the durable log necessary. Bounded by `--timeout` because
harnesses impose tool timeouts.

**2. Stop-hook injection** — the working-agent case. The harness adapter runs
`agent-bus read --json` at the turn boundary; if unread messages exist it exits
non-zero with them rendered as instruction text, and the agent continues instead
of going idle. Because delivery is cursor-gated, an agent is only re-continued
for genuinely unread messages — no wake loops.

Interrupt-vs-queue policy is carried in the message (`priority`) and rendered by
the adapter, not baked into the daemon. Receiver instructions decide what to do
with it.

**Adapters** live outside the core: one file per harness (Claude Code `Stop`
hook, OpenCode plugin), each a thin call into `read`. Adding a harness costs one
small file and no daemon changes.

## `agent-bus guide`

Written for a model that has never seen the tool. Not a flag dump — the
conceptual model (partitions, cursors, why `wait` is free), the two delivery
modes and when to pick each, three worked examples including the dev→reviewer
handoff, and the exit-code contract. `--help` stays conventional and short with
a pointer to `guide`.

## Error handling

No silent failures. Unwritable state dir, full disk, or socket permission
problems surface as a non-zero exit with an actionable message naming the path.
Corrupt log lines are skipped with a warning to stderr and a count in `status` —
skipped, never swallowed. A `post` returns only after the message is durably
appended and `fsync`ed.

Cursors that point at pruned messages snap forward to the oldest surviving id;
`status` reports when this happened so "my agent missed messages" is diagnosable.

## Testing

**Core (unit):** wildcard matcher including `*` vs `**` depth semantics and bare
partition shorthand; partition extraction; retention boundary; cursor advance
and snap-forward; ULID ordering.

**Integration (temp socket, temp state dir):**
- post-before-subscribe → `wait` returns immediately (the core race)
- daemon restart → subscriber resumes at cursor, no loss, no replay
- partition isolation → `other/x` invisible to `iot_base/**`
- concurrent auto-start → N simultaneous clients, exactly one daemon
- `wait` timeout → exit 2, cursor unmoved
- retention prune → old messages gone, cursors snapped, no crash
- killed client mid-delivery → message redelivered, not dropped

## Rust practices

Edition 2024. `#![forbid(unsafe_code)]`. `clippy::pedantic` clean.
`thiserror` for library errors, `anyhow` at the binary boundary. `clap` derive
for the CLI. `tokio` in the daemon only — core and CLI stay sync. `serde` for
the wire format. No `unwrap()` outside tests.
