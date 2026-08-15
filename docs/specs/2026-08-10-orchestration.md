# agent-bus Orchestration — Design

**Date:** 2026-08-10
**Status:** Proposed
**Depends on:** [`2026-08-06-agent-bus-design.md`](2026-08-06-agent-bus-design.md), [`2026-08-07-dashboard.md`](2026-08-07-dashboard.md)

## Purpose

Coordinate multiple AI coding agents across multiple projects as a team. The
bus stays the transport under the hood; on top of it we build a **per-agent
message queue** (post to an agent at any time, delivered when the agent reaches
a turn boundary — like a human typing and hitting enter), a **state model**
(working / idle / waiting for input / blocked / error) observed from the
sessions themselves, and a **workflow engine** (a DAG of tasks with
dependencies) that routes work between agents.

The system makes as many decisions as possible **offline, with zero LLM calls**,
from deterministic rules written in code. Only decisions the rules cannot make
with confidence are escalated to a **manager agent** (an LLM). The manager logs
every exception it handles, and over time those exceptions are **baked back**
into the in-code rules — a closed loop that makes the system more robust and
more automatic with every unusual case it survives.

## Principles

1. **The bus is the substrate.** Messages, state-change events, and workflow
   events all flow through agent-bus. No new transport. The daemon stays dumb:
   it relays and retains, it never decides.
2. **Queue, don't block.** A message to a busy agent is enqueued, not dropped
   and not interrupting. Delivery is at the agent's next turn boundary, in
   queue order. Senders never wait on the transport; "did the agent do it" is a
   workflow concern (acks), not a transport concern.
3. **LLM-minimal by default.** The rule engine and DAG engine run offline.
   Confidence decides: act, or delegate to the manager.
4. **When in doubt, delegate.** No silent guesswork. If no rule matches, or
   rules conflict, the decision goes to the manager with a structured question
   and full context. An agent that made a wrong guess is worse than one that
   asked.
5. **The loop is closed.** Every delegated decision is logged. A bake-back pass
   turns recurring decisions into new rules, so the same edge case is handled
   in code next time. The system improves itself; the manager's judgment
   becomes the system's default behavior.
6. **Humans see everything.** A centralized view (the dashboard) shows every
   agent's state, last message, and pending workflow in one place.

## Core model

- **Agent** — a named entity that runs in a terminal surface (or a pure
  worker). Has an identity (`dev_01`), a role/type (`dev`, `tester`,
  `designer`, `manager`), a **state**, and an **inbox**.
- **Inbox** — the per-agent ordered queue. Messages addressed to an agent
  accumulate here and are drained at the agent's turn boundary. Backed by the
  same durable log + cursor machinery as today's patterns, keyed by recipient.
- **Message** — the unit of coordination. Extends today's schema with
  recipient addressing, kinds, and acks (below).
- **Signal** — an observed fact: `lifecycle.idle`, `process.exit(1)`, an output
  snapshot, a message posted. Produced by the **observer**, consumed by the
  rule engine.
- **State** — the agent's current condition (`working`, `idle`, `error`, ...),
  with a provenance (`observed` vs `inferred`) and a confidence. Every state
  change is itself posted to the bus as an event.
- **Rule** — a deterministic if-then in code/data that maps signals and context
  to a decision: a state transition, a route, a workflow trigger. No LLM.
- **Workflow (DAG)** — a declarative graph of tasks. Nodes have a owning role,
  dependencies, a start-message template, and a completion criterion. Driven
  offline by the DAG engine.
- **Manager agent** — the LLM. Only invoked by the decision cascade for
  low-confidence or uncovered situations. Writes a **decision log**; the
  **bake-back** pass turns that log into new rules.

## Message schema

Today's schema plus:

```json
{
  "id": "01JQ8F2K9X3M4N5P6Q7R8S9T0V",
  "ts": "2026-08-10T09:15:00.000Z",
  "topic": "iot_base",
  "priority": "high",
  "from": "manager",
  "to": "dev_01",
  "kind": "instruction",
  "in_reply_to": null,
  "ack": null,
  "body": "implement auth per spec ref iot_base/specs/auth.md"
}
```

- `to` — recipient agent id. Absent means the message is topic-addressed
  (existing first-pick-up semantics). `to` and topic pattern can coexist: a
  copy addressed to the recipient plus topic routing for observers.
- `kind` — `instruction` | `request` | `answer` | `ack` | `state` | `event` |
  `decision_request` | `decision_response`. Acks and answers are routing
  handled by rules, not by the transport.
- `ack` — for a completion ack: the task id being acknowledged, e.g.
  `dev_01` acks `dev.done`. This is how the DAG learns a node finished.
- `in_reply_to` — links an answer/ack to the message that asked. Lets the rule
  engine route replies back to the asker deterministically.

**Addressing modes over one log:** *topic* (pub-sub, first pick-up wins —
today's model) and *recipient* (point-to-point, per-recipient queue — new).
Both share retention, durability, and cursor mechanics.

## Delivery: post and queue

Posting to an agent never blocks and never interrupts:

```
sender --post --to dev_01--> daemon --> dev_01.inbox (durable log)
                                          |
          dev_01 reaches turn boundary <---+
          hook drains inbox (in order, high-priority first)
          agent acts, may ack
```

- **Queued while busy.** If `dev_01` is mid-turn, the message waits. No
  mid-tool-call interruption — the honest guarantee stays "delivered at the
  next turn boundary", the same contract agent-bus already has.
- **Order.** Within an inbox, delivery is in queue order (ULID), with `high`
  priority pulled ahead. A sender can rely on FIFO for the normal case.
- **No transport acks.** The sender learns nothing from `post`. Completion is
  a workflow event: the agent posts `kind=ack, ack=task_id`, and whoever asked
  (a rule, a DAG, or another agent) reacts.
- **Idle agents.** If an agent is idle, the hook delivers immediately at the
  turn boundary — which for an idle agent is essentially now. Queue depth zero.

The existing `wait` primitive still exists for the dedicated-waiter case
("wait until the reviewer's ack, then act") at zero token cost. Queued delivery
is the default for agents that are busy with other work.
## State model

States are kept deliberately small. `done` is not a state — it is a workflow
fact carried by acks.

| State | Meaning | Provenance |
|---|---|---|
| `unknown` | never seen, or unreadable | observed |
| `spawning` | surface created, agent booting | observed |
| `working` | agent is running a turn (activity) | observed |
| `idle` | finished a turn, no pending background work | observed |
| `waiting_input` | needs a human/manager decision to continue | observed |
| `blocked_permission` | waiting for a tool-permission approval | observed |
| `error` | process exited non-zero, crashed, or error markers | observed / inferred |

**Transitions** are produced by the rule engine from signals:

| Signal | Transition |
|---|---|
| `lifecycle.working` / activity burst | → `working` |
| `lifecycle.idle` (no bg task pending) | → `idle` |
| `lifecycle.needs_input` | → `waiting_input` |
| `lifecycle.blocked_permission` | → `blocked_permission` |
| `process.exit(0)` | → `idle` |
| `process.exit(n≠0)` / crash | → `error` (observed) |
| output matches error pattern | → `error` (inferred, low confidence) |
| recovery signal (new turn starts) | → `working` |

Every transition is posted to the bus (`kind=state, body={agent,from,to}`), so
the DAG engine, the dashboard, and other agents all subscribe to the same
stream. Confidence and provenance ride along, so consumers can weight inferred
states lower than observed ones.

## Observation

A long-lived **observer** produces signals from the sessions themselves:

- **cmux socket API** — read the latest output lines of any surface, list
  workspaces/panes, sidebar state (status pills, progress, agent activity
  spinner), notifications.
- **cmux agent hooks / notifications** — `lifecycle.idle`,
  `lifecycle.needs_input`, `blocked_permission` map almost one-to-one to
  cmux's own agent lifecycle signals (the same ones its idle reminders and
  notification policy already use).
- **Process liveness** — the surface's process exiting with a code is the most
  reliable error signal there is (observed).
- **Output scanning** — heuristics over the last N lines: error patterns per
  agent type, "waiting for your input" prompts, test-result banners. Always
  *inferred* and confidence-tagged; never authoritative on their own.

Signals are cheap facts. They are batched and pushed to the rule engine; the
observer never decides anything.

## Decision cascade

```
signal in
   │
   ▼
rule engine (offline, no LLM)
   │
   ├─ match, confidence ≥ threshold  → act (transition, route, trigger)
   ├─ no match, or conflict          → escalate to manager
   └─ no signal change               → stay; nothing to decide
   │
   ▼
manager agent (LLM, only here)
   decides, posts action, writes decision log
   │
   ▼
bake-back (periodic, human-gated)
   recurring decisions → proposed rule → approved → merged into rules file
```

- **Act.** Deterministic, fast, auditable. Rules live in code/data and are
  reloadable (`agent-bus rules reload`).
- **Escalate.** The cascade posts a `decision_request` to the manager's inbox
  with a structured question: the situation signature (state, signals, recent
  messages, workflow node), the candidate actions the rules considered, and the
  context refs (last output, log window). The manager replies with a
  `decision_response`; the orchestrator executes it and logs it.
- **Delegation is always visible.** Every `decision_response` lands in the
  decision log and is surfaced on the dashboard as "manager deciding: …".

Rules come in two kinds. **Data rules** are the default — bake-back and
hot-reload operate on them directly. **Code rules** are small Rust functions
registered at build time for cases the evaluator cannot model (complex state,
cross-references, anything requiring real logic). Both return a decision with
a confidence and are scored in the same cascade.

Data rule shape:
[[rule]]
id = "rerun_crashed_tester_once"
when = { agent.type = "tester", state = "error", reason = "exit",
         times_errored_in_1h = { lte = 1 } }
confidence = 0.9
action = { kind = "post", to = "$agent", body = "Your last run crashed \
          (context below). Re-run once from the same point.\n{last_output}" }
```

## Workflows (DAG)

A workflow is a declarative graph. The engine is offline: it watches the
state/ack event stream, advances nodes whose dependencies are complete, and
posts the start message for the next ready node. No LLM in the happy path.

```toml
[[node]]
id = "design"
role = "designer"
start = "Design {feature} per spec ref {spec}; when done, ack design.done"
done_when = { ack = "design.done" }

[[node]]
id = "dev"
role = "dev"
depends_on = ["design"]
start = "Implement {feature} from the design at {design.ack}; ack dev.done"
done_when = { ack = "dev.done" }

[[node]]
id = "test"
role = "tester"
depends_on = ["dev"]
start = "Test {feature}; report results; ack test.done"
done_when = { ack = "test.done" }
```

Node states: `pending`, `ready`, `running`, `blocked`, `done`, `failed`,
`needs_decision`. Completion is normally `done_when` (an ack, a status message,
or an output pattern). Ambiguity — no ack, timeout, conflicting signals —
moves the node to `needs_decision` and delegates (the manager may say "done",
"rerun once", "skip", "split").

`on_error` per node: `rerun` (bounded), `skip`, `delegate` (default — the
manager decides). Bounds on reruns are in code so the manager cannot spin.

The engine posts workflow events (`node.ready`, `node.done`, `node.failed`)
to the bus like everything else. A human or another agent can subscribe to the
whole team's progress.

## The learning loop

- **Decision log.** Append-only file per manager run: situation signature,
  context refs, decision, outcome (optional), timestamp.
- **Bake-back.** A pass (manual or scheduled, always *previewed*) clusters the
  log by situation signature. Signatures that recur enough times become
  **proposed rules** with an initial confidence. A human (or the manager with
  human sign-off) merges them into the rules file. Rules can always be
  reverted; nothing is automatic-by-default in the first iteration.
- **Robustness is the goal.** Every delegation is an investment: it makes the
  rule engine slightly more complete. Over time the manager is asked less and
  the system handles the long tail of edge cases in code.

## Data flows

**1. Post and queue.**

```
user / agent / DAG engine
   │  post --to dev_01 {kind:instruction}
   ▼
daemon ──append──> log (durable)     dev_01 busy → message waits
   ▲
dev_01 turn boundary: hook drains inbox ──delivers next──> dev_01 acts
```

**2. State observation.**

```
cmux surface (dev_01)
   │  socket read / lifecycle hook / process exit
   ▼
observer ──signals──> rule engine ──transition──> state store
                                                   │
                                                   └─post kind=state──> bus
```

**3. Workflow drive.**

```
bus: ack dev.done ──> DAG engine: mark dev done
                          │
                          └─ node test becomes ready
                                └─ post --to tester {start test}──> tester inbox
```

**4. Decision cascade.**

```
rule engine: no confident rule ──> post decision_request ──> manager inbox
                                                                  │
            manager decides ◄── (LLM, reads context refs) ◄────────┘
                  │
                  ├─ post decision_response ──> orchestrator executes
                  └─ append decision log
```

**5. Bake-back.**

```
decision log ──> bake-back (preview) ──> proposed rule ──> human approves
                                                               │
                                          merge into rules file ─┘
                                          → reload, next time it's offline
```

## Worked examples

**A. The test→dev chain (the DAG happy path, zero LLM).**

1. `tester` acks `test.done`. The ack is a bus message.
2. DAG engine marks `test` done, sees `dev` (and downstream) become ready.
3. Engine posts the `dev` start message to the `dev` agent's inbox.
4. `dev` is mid-turn; the message queues. At its next turn boundary the hook
   delivers it and `dev` starts. No manager, no LLM call.

**B. Human types while the agent is busy.**

1. You post `"before you continue, explain why the migration is unsafe"` to
   `dev_01` while it's mid-refactor.
2. It lands in the inbox. `dev_01` finishes its current step, hits the turn
   boundary, receives the question, answers.
3. The answer is a `kind=answer, in_reply_to=…` message. A rule routes it back
   to your inbox/notification. The work paused exactly like a human would
   pause — no interrupt, no loss.

**C. An edge case nobody encoded yet (the loop in action).**

1. `tester` process exits non-zero → observer signal → `error` (observed).
2. Rule engine has no rule for this tester/state/context signature → no
   confident match → escalate.
3. Manager reads the last output, decides "the fixture port collided; rerun
   with `--port 0`", posts the instruction, logs the decision.
4. Bake-back preview clusters this signature; a rule is proposed: "tester
   crashed with `port collision` in output → rerun once with `--port 0`".
5. Approved and merged. The next occurrence is handled offline in ~0s.

## Architecture

One new binary; the existing crates grow narrowly.

- **`agent-bus-core`** — recipient addressing and cursor arithmetic, message
  kinds/acks, the state model, rule matching, DAG state logic. All pure, no
  I/O, no async — same contract as today.
- **`agent-bus-daemon`** — unchanged role (transport, log, cursors, retention).
  Gains only per-recipient delivery as an addressing mode. Stays dumb.
- **`agent-bus-orchestrator`** *(new)* — the brain. Long-lived, like the
  daemon: owns the observer (cmux socket + hooks), the state store, the rule
  engine, the DAG engine, decision-log writing, and bake-back. Talks to the
  daemon over the socket and to cmux over its socket. Auto-started by CLI
  commands, idles out, restarts, same lifecycle pattern as the daemon.
- **`agent-bus-cli`** — new subcommands for humans and hooks: post-to,
  inbox/state views, orchestrator control, DAG and rules management, and the
  dashboard extended to render agent states.

The orchestrator is a separate process so the daemon never grows a brain and
the two can restart independently. If the orchestrator is down, the bus still
relays messages — agents and humans keep working, only the automatic parts
pause.

## Command surface (proposed)

```
agent-bus post --to dev_01 "…"             post to a specific agent's queue
agent-bus agents                            list agents, states, queues
agent-bus agent show dev_01                state, last message, inbox depth
agent-bus inbox dev_01                     inspect a queue (human, read-only)
agent-bus orchestrator status              engine health, rule count, backlog
agent-bus orchestrator start/stop
agent-bus dag apply <file>                 validate + install a workflow
agent-bus dag status [id]                  node states for a workflow
agent-bus rules list | reload              inspect / hot-reload rules
agent-bus manager bake-back --preview      propose rules from decision log
agent-bus dashboard                        extended: agent state board
```

## Non-goals

- Network transport, multi-user, cross-machine coordination.
- Guaranteed exactly-once delivery or sender-side transactionality. The log is
  at-least-once with cursor gating, as today; idempotency for actions is the
  agent's job (task ids in `ack`).
- Scheduling/slotting of agents, resource management, or spawning agents — that
  stays with cmux. The orchestrator observes and instructs; it does not own the
  processes.
- Fully automatic bake-back. Every rule that changes behavior starts as a
  preview and needs sign-off (human or manager-with-human-gate).
- Auto-decoding arbitrary agent output into structured meaning. We scan for
  patterns and exit codes; deep understanding is the manager's job.

## Decisions (resolved)

1. **Rules: both data and code.** Rules are data (TOML) when the evaluator can
   express them — the default, because it makes bake-back and hot-reload
   trivial. But there will always be exceptions the evaluator cannot express,
   so the engine also accepts **code rules**: small Rust functions registered
   in the orchestrator (or core) that participate in the same cascade with the
   same confidence contract. Data rule wins ties; code rules exist precisely
   for the hard-to-model cases.
2. **Bake-back: human-gated first, relaxing over time.** Every rule that
   changes behavior starts as a `--preview` and requires human sign-off. Once
   the process has history and the human trusts the clustering, the gate can
   be loosened to allow the manager to self-approve low-risk rules (confidence
   + signature-matches-enough) within a bounded trust window. The gate is a
   knob, not a rewrite: one setting controls how autonomous bake-back is.
3. **State store: event-sourced in-memory map, log on disk.** The orchestrator
   keeps the authoritative store as `HashMap<AgentId, AgentRecord>` in memory
   — O(1) lookups, trivially fast for dashboards and `agent show`. Durability
   and rebuild come from an append-only **state event log** (transitions,
   acks, decisions) replayed on start, exactly like the bus's own log-first
   philosophy. One owner: the orchestrator. The dashboard and CLI read from
   it; they never hold a second source of truth.
4. **Manager: one global manager, project-aware.** A single manager inbox and
   agent. Project context is *scoped*, not global: each escalation carries the
   project's workflow + node states, the relevant rules/overrides for that
   project, and compact context refs (see *Manager context* below). The
   manager offloads depth to subagents on demand instead of carrying every
   project's detail in its window.
5. **Observer transport: cmux socket API + hooks now.** `cmux-agent-mcp`
   (PolyForm Strict) is explicitly not required; it remains an optional later
   backend if we ever want MCP-native tooling.
6. **Confidence thresholds: fixed per rule now.** Learned from bake-back
   outcome data later, once the decision log has enough history to train on.

## Manager context

The manager is one agent; its window is a scarce resource. The orchestrator
builds every escalation as a **compact situation record**, not a dump:

- **Identity:** escalation id, timestamp, originating project.
- **Situation signature:** agent, state, signals that fired, node id (if
  inside a workflow), and the candidate actions the rules considered.
- **Refs, not payloads:** pointers to the context the manager can pull if it
  needs depth — last N output lines, log window, spec file path, inbox state.
  The manager fetches refs on demand (via its tools); it never receives a
  wall of terminal text by default.
- **Project scope:** only this project's active workflow, node states, and
  rule overrides. Other projects' state never appears in the record.

**Subagents for depth.** When a decision needs project-specific detail (read a
spec, inspect a repo), the manager spawns a subagent to fetch it rather than
carrying project knowledge in its own context. This is how one manager stays
project-aware across many projects without accumulating their context.

**Pollution guard.** The manager's own inbox holds only decision requests,
human messages, and its acks. Workflow events and state changes go to the
bus, not to the manager's inbox, unless a rule explicitly routes them there.
If the manager's window grows beyond a threshold, the orchestrator truncates
the oldest context (keeping the escalation signature) and notes the truncation
in the log.

## Dashboard: needs-input triage

The dashboard (TUI, extended from `2026-08-07-dashboard.md`) is the human's
view of every agent that needs an answer. When an agent — including one a
human started by hand, never registered with the bus — reaches
`waiting_input` / `blocked_permission`, it appears at the top of the board
with its last output snippet:

- **Reply inline.** Select the row and type an answer. The dashboard posts it
  with `post --to <agent>`; the agent picks it up at its next turn boundary
  exactly like a human typing in its terminal. No state-machine ceremony, no
  manager involved.
- **Jump to the terminal.** Select a row and hit return to focus that agent's
  cmux workspace/pane via the cmux socket API (`select_workspace` /
  `focus_pane`), so the human can answer directly in the terminal if they
  prefer. Both are cheap and both use existing primitives: the bus for the
  reply, cmux's socket for the jump.

Triage rows are driven by the state-event stream, so they appear and clear as
transitions happen — no polling in the TUI.

## Addendum 2026-08-13 — OpenCode as the control plane

This addendum revises the agent surface, observation, and delivery sections in
light of the opencode server/client architecture (v1.18+). The bus remains the
coordination substrate; cmux remains the launcher and the human's terminal.
What changes is *how the orchestrator sees and reaches each agent*: every agent
is an opencode instance exposing its own HTTP server, and the orchestrator
drives and observes it over that API instead of scraping cmux panes.

### Agent surface and launch (revises "Core model", "Architecture")

Each agent is one opencode instance, launched by the orchestrator via the cmux
socket API into its own workspace/pane:

```
cmux API → spawn workspace "<project>_team", pane "dev_01"
            opencode --port 4101 --hostname 127.0.0.1 --dir <project> \
                     --agent dev --session <persistent-session-id>
```

**Workspace layout is per-project, not global.** A workspace is a cmux
workspace holding that project's panes. Layouts differ project to project —
`iot_base` may have `dev_01, reviewer_01, manager`, while another project runs
`planner, dev_01, tester, reviewer_02, manager`. The **roster** (which agents,
roles, models, and pane arrangement) is declared per workspace in a layout file
(`agent-bus orchestrate up iot_base` reads `agent-bus.toml`); nothing is hard-
coded in the orchestrator.

**Port assignment is per-workspace allocation, not a global formula.** Because
panes are addressed by agent id (`dev_01`) and two workspaces can both have a
`dev_01`, the port cannot be derived from the agent id alone. The orchestrator
keeps a per-workspace **port table**: on first `up`, it allocates the lowest
free loopback ports in a configured range (e.g. `4100..4299`) for the roster's
panes, and records `(workspace, pane) → port`. On subsequent `up` it reuses the
recorded port, so restarts are stable and no discovery is needed.

- **`--port` keeps the TUI.** `opencode --port 4101` binds the HTTP server
  *and* renders the normal TUI, so a human can sit at any pane and type input
  exactly as today. The same instance is reachable at `127.0.0.1:4101`. Plain
  `opencode` (no flags) binds nothing — the fixed port is what makes the agent
  reachable, so it is always passed.
- **Session identity is tracked and persisted.** Each pane's opencode session
  id is recorded in the same per-workspace table as its port. On restart, the
  orchestrator relaunches with `--session <recorded-id>` so the agent resumes
  its exact conversation, context, and todo state. A fresh pane that never
  completed a session gets a new one (`POST /session`) and the id is stored
  before the process is told to run. If a pane's session id is lost or gone,
  the orchestrator falls back to a new session and notes the truncation in the
  log — mirroring the manager's pollution-guard rule.
- **cmux owns processes, not brains.** Launch, respawn on crash, teardown, and
  the visible terminal stay with cmux. The orchestrator never pokes a pty; it
  only calls the agent's HTTP API.
- **Managed launch.** `agent-bus orchestrate up <workspace>` launches the whole
  roster via cmux; `orchestrate down` tears it down and releases the port
  table. Launch is idempotent: agents already up are left running, and their
  recorded session id is re-read rather than overwritten. Orchestration is thus
  *automated* but the processes themselves are still cmux's, per the non-goal.

### Observation (revises "Observation")

The observer subscribes to each agent's **SSE event stream** (`GET /event` on
its own server) instead of scraping cmux panes. Signals become structured and
observed:

| opencode event | State transition |
|---|---|
| `session.next.step.started` | → `working` |
| `session.status` = `busy` | → `working` |
| `session.next.step.ended` | → `idle` |
| `session.idle` | → `idle` |
| `session.status` = `retry` | → `working` (retry logged) |
| `session.next.step.failed` / `session.error` | → `error` (observed) |
| `permission.asked` | → `blocked_permission` |
| `session.next.tool.failed` ×n | → `error` candidate (low confidence) |

Output-scanning heuristics demote to a **fallback** for surfaces opencode does
not own. `GET /session/status` (returns `idle | busy | retry` per session) is a
cheap recovery path if an SSE stream drops.

### Delivery (revises "Delivery")

The orchestrator holds each agent's inbox (the queue model above) and is the
*only* drainer that talks to opencode:

```
DAG / rule / human → post --to dev_01 → dev_01.inbox (bus, durable)
orchestrator: dev_01 idle? → POST /session/:id/prompt_async
                            → 204, opencode queues serially
                            → delivered at next turn boundary
```

- opencode **natively queues** a prompt against a busy session (one assistant
  step at a time), so "delivered at the next turn boundary" is inherited, not
  emulated. `prompt_async` returns immediately — post never blocks.
- **Priority stays in the inbox**, not on the wire: the orchestrator drains
  `high` first and holds `normal` until the agent is idle.
- **No per-agent plugin hook.** The `session.idle`-based delivery plugin is
  dropped; the orchestrator's event subscription replaces it.
- **Acks unchanged.** The agent still posts `kind=ack, ack=task_id` on the bus;
  the DAG advances on that, not on any opencode response.

### Permissions (new capability)

`permission.asked` arrives on the event stream with a `permissionID`. The rule
engine maps it: known-safe → auto-allow via
`POST /session/:id/permissions/:permissionID`; known-dangerous → deny; unknown →
escalate (manager or dashboard human). This makes `blocked_permission` fully
actionable instead of merely visible.

### Manager (revises "Manager context")

The manager is a dedicated opencode session with a `manager` agent config.
`decision_response` is produced with **structured output** (`json_schema` on the
prompt), so the reply arrives as validated JSON the orchestrator executes
directly — no free-text parsing. Depth work stays with native subagents (session
fork / Task tool).

### Human input (revises "Dashboard")

The TUI is always present in every pane (`opencode --port`), so "jump to
terminal" is just cmux `focus_pane` on the agent's workspace. "Reply inline"
still posts `post --to <agent>`; the orchestrator drains it to the same session.

### Architecture delta

- `agent-bus-orchestrator` gains two HTTP clients: one per agent's opencode
  server (generated from its `/doc` spec), plus the existing cmux socket for
  launch. Drops cmux output-reading and output-scanning as primary observation.
- `agent-bus-core` — unchanged surface plus the **workspace table**:
  `(workspace, pane) → { port, opencode_session_id }`, allocation logic, and
  the opencode-event → signal mapping. Pure, as always.
- `agent-bus-daemon` — unchanged, still dumb.
- `agent-bus-cli` — adds `orchestrate up|down|status` and a per-workspace
  layout file (`agent-bus.toml`) listing the roster and pane arrangement.

### Decisions (resolved, 2026-08-13)

7. **Every agent is an `opencode --port` instance.** Per-pane ports make the
   agent reachable by URL while keeping the human TUI. cmux launches and
   supervises the processes; the orchestrator never touches the pty.
8. **Observation is opencode's event stream; cmux output scanning is the
   fallback.** Structured `session.*` / `permission.asked` events are the
   primary signals; `GET /session/status` covers SSE loss.
9. **Delivery is `prompt_async` from the orchestrator's inbox drain.** opencode
   serializes prompts per session, so queue-then-deliver-at-turn-boundary is
   inherited; priority and acks stay in the bus inbox.
10. **The workspace table is the launch truth.** Per-workspace layout (roster +
    pane arrangement) is declared in a layout file, not hard-coded. Ports are
    allocated per workspace in a configured range, and each pane's opencode
    session id is persisted alongside its port — so restarts resume the exact
    session and allocation is stable. Both live in the orchestrator's
    event-sourced state, replayed on start.

### Non-goals (updated)

- The bus does not proxy opencode's API; the orchestrator addresses each
  agent's server directly over loopback fixed ports.
- No dependency on opencode's v2 instance model; this works on v1.18+, where
  `opencode --port` exposes the server with the TUI intact.

## Testing

**Core (unit):** recipient-address parsing and per-recipient cursor math;
message kinds and ack links; state transition table (every signal × state —
transitions not in the table are rejected); rule matching (match / no-match /
conflict / confidence below threshold); DAG readiness computation from a
partial node set; rerun bounds.

**Orchestrator (integration, real daemon + fake cmux):** post-to while the
agent is busy → message stays queued, delivered at the simulated turn boundary
in order; priority pre-emption; ack → DAG advance → next start message posted;
uncovered signal → `decision_request` posted, `decision_response` executed and
logged; bake-back clustering produces a correct proposed rule from a synthetic
decision log; two workspaces with overlapping pane ids allocate distinct ports;
orchestrator restart replays the workspace table and relaunches each pane with
its recorded session id (no reallocation, no new session).

**Bus (unchanged):** existing integration suite stays green; new case for
`post --to` across a daemon restart (resume at the recipient cursor, no loss).

## Rust practices

Unchanged from the workspace contract: edition 2024, `unsafe_code` forbidden,
`clippy::pedantic` clean, `thiserror` in core only, `anyhow` at binary
boundaries, `tokio` in long-lived binaries (daemon, orchestrator) only, `serde`
on the wire, no `unwrap()` outside tests. The rule evaluator and DAG engine
are pure and live in core so the whole decision surface is unit-tested.
