# agent-bus

A local event bus for coordinating AI coding agents that run in separate
terminal sessions.

A dev agent finishes work and posts a message. A reviewer agent in another
terminal picks it up and starts reviewing. No broker to install, no
configuration, no ports — the first command you run starts the daemon.

```bash
# Terminal 1 — the dev agent, when its work is done
agent-bus post iot_base/dev_01 "feature XYZ done, commit abc123, ready for review"

# Terminal 2 — the reviewer agent, blocking at zero token cost
agent-bus wait 'iot_base/**' --as reviewer_01 --timeout 30m
```

The reviewer receives the message even if it started waiting *after* the post.
That is the point of the design: messages are durable for an hour, so agents
started at uncoordinated times never miss each other.

## Install

```bash
cargo install --path crates/cli
cargo install --path crates/daemon
```

Both binaries need to be on `PATH`; the CLI starts the daemon automatically.

## Model

**Partitions.** The first topic segment (`iot_base` in `iot_base/dev_01`) is a
hard isolation boundary. Multiple projects, each with several agents, can share
one machine without ever seeing each other's traffic.

**Wildcards.** `*` matches exactly one segment, `**` matches one or more, and a
bare partition name means the partition and everything in it. The first segment
must be a literal — a wildcard there would escape the partition.

**Delivery.** The default is exclusive per pattern: each pattern has one read
position shared by every consumer, and the first consumer to read a message
takes it — it is never delivered again to anyone, even another consumer of the
same pattern. A pool of reviewers sharing one pattern therefore splits the
work, first-to-pick-up wins, with no coordination. Post with `--broadcast` to
instead deliver a message to every consumer (distinct `--as` label) whose
pattern matches, each getting their own copy once. `--as` labels are cosmetic
for exclusive delivery but are the identity that scopes broadcast delivery.

**Retention.** Messages live for 1 hour. The daemon exits after 1.5 hours idle —
deliberately longer, so by the time it goes the logs hold nothing of value — and
restarts on the next command.

## Commands

| Command | Purpose |
|---|---|
| `daemon <pattern>` | Ensure the daemon and partition exist (idempotent) |
| `post <topic> [msg]` | Publish; reads stdin if the message is omitted |
| `wait <pattern>` | Block for one unread message, print it, exit |
| `read <pattern>` | Print all unread and exit immediately |
| `follow <pattern>` | Stream continuously |
| `history <pattern>` | Replay, ignoring cursors |
| `status` | Partitions, message counts, pattern positions, lag |
| `stop` | Shut the daemon down |
| `guide` | Full usage guide, written for AI agents |
| `hook install <harness>` | Wire up Claude Code or OpenCode delivery |
| `dashboard` | Live TUI: per-minute throughput, latency "time to pick up", message sizes, gauges, partition tables (quit with `q`) |

Add `--json` to any command for machine-readable output. Add `--broadcast` to
`post` to deliver a message to every consumer whose pattern matches, rather
than exclusively to the first one that reads it.

## The two ways to receive

**Blocking `wait`** — for an agent whose job is to wait for something. It costs
**zero tokens while blocked**: one tool call that returns once, not a polling
loop. Use it when a reviewer is waiting on a dev.

**Hook delivery** — for an agent that is busy with other work.
`agent-bus hook install claude-code 'iot_base/**' --as dev_01` installs a hook
that drains unread messages at each turn boundary.

Neither Claude Code nor OpenCode can interrupt an agent mid-tool-call, so
"delivered at the next turn boundary" is the honest guarantee.

## Exit codes

Agents and shell loops branch on these, so they are a stable contract.

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Usage error (bad topic, bad pattern, bad arguments) |
| 2 | `wait` timed out with nothing delivered |
| 3 | Daemon unreachable and could not be started |

Code 2 is reserved exclusively for a `wait` timeout, which makes this safe:

```bash
while agent-bus wait 'iot_base/**' --as reviewer_01; do
  : # handle the message
done
```

## For agents

```bash
agent-bus guide
```

Prints a guide written for an AI agent with no prior knowledge of the tool — the
conceptual model, when to use each delivery mode, worked examples, and the exit
code contract. Point your agent's context at it.

## Development

```bash
cargo test --workspace                      # 140 tests
cargo clippy --workspace --all-targets      # pedantic, warning-free
cargo fmt --all -- --check
```

Integration tests drive the real binaries over a real Unix socket, with an
isolated state directory per test so they run in parallel.

Two environment variables exist for testing and unusual setups:
`AGENT_BUS_STATE_DIR` overrides where state lives, and `AGENT_BUS_DAEMON_BIN`
points the CLI at a specific daemon binary.

Design notes: [`docs/specs/2026-08-06-agent-bus-design.md`](docs/specs/2026-08-06-agent-bus-design.md)
