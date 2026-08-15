# Fleet Supervisor — Graph Engine v2: Detailed Design (P1–P7)

**Status:** Draft (build blueprint — component-by-component, implementable)
**Date:** 2026-08-14
**Revision:** reviewed 2026-08-14; 9 findings applied (G1–G9, see §14)
**Depends on:** `2026-08-13-supervisor-detailed-design.md` (implemented),
`2026-08-14-supervisor-wiring-fixes-design.md` (Phase A),
`2026-08-14-supervisor-webui-detailed-design.md` (Phase B)
**Audience:** dev agent, designer

This document is the result of comparing our workflow/graph engine against
LangGraph's graph-execution runtime (state + reducers, step checkpointing,
interrupts, `Send` fan-out, subgraphs, conditional edges, streaming). It
specifies the seven improvements (P1–P7) we chose to lift into our own core,
phased so the cheap, high-value ones land with the current work and the
structural ones land after the web UI v1.

**Phasing:**

- **Phase A2 — now, with the wiring fixes (F/M)**: P1 (typed state), P2 (step
  records + rewind), P7 (transition streaming). Pure-core + runner + small API
  additions; backward compatible; directly enables the web UI's live canvases,
  timeline, and "visualize a running process".
- **Phase B2 — graph engine v2, after web UI v1**: P3 (interrupt + resume),
  P6 (conditional routes), P5 (subgraphs), P4 (runtime fan-out) — in that
  order. P4 last: it builds on P2's step records and complicates restart
  restore, so it must not start before M3 (restart restore) is proven.

All changes stay in `supervisor-core` (pure) and `supervisor-daemon`
(runner/persistence/API). The `#[serde(default)]` additions to `NodeDef` are
backward compatible; the DAG editor must not freeze its node schema before
these fields exist (§12).

---

## 1. P1 — Typed node inputs/outputs (workflow state)

**Lesson from LangGraph.** Nodes share a typed state dict with reducers; a
node's output lands in state and any downstream node consumes it. Ours only
has `{feature}`-style string vars plus the ACK `summary` string.

### 1.1 Core data shapes (`supervisor-core/src/dag.rs` + new `state.rs`-side types)

```rust
/// The accumulated data state of one workflow run (P1). Journaled with every
/// step (P2) so a restarted daemon rebuilds dataflow, not just node states.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowState {
    /// Workflow-level variables: inputs at start, plus `{node_id}` outputs
    /// merged in on node completion.
    #[serde(default)]
    pub vars: BTreeMap<String, serde_json::Value>,
    /// Per-node completion records: task_id → what that node finished with.
    #[serde(default)]
    pub acks: BTreeMap<String, NodeAck>,
}

/// One node's recorded completion (P1). `payload` is the node's typed output:
/// the structured field when the driver had one, else the parsed ACK JSON,
/// else `{ "summary": … }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeAck {
    pub node: String,
    pub status: AckStatus,
    pub summary: Option<String>,
    pub approved: Option<bool>,
    pub needs_revision: Option<Revision>,
    pub payload: Option<serde_json::Value>,
}
```

`Workflow` gains `state: WorkflowState` (private, exposed via
`workflow_state() -> &WorkflowState` — named to avoid colliding with the
existing per-node `state(id) -> Option<NodeState>` accessor; Rust has no
overloading). `Workflow::new(graph)` starts empty.

### 1.2 Completion writes state

`Workflow::apply_ack(&mut self, ack)` — before the existing state machine
logic, record the ack against the node(s) it completes:

```
workflow.record(node, ack) =>
  state.acks[task_id] = NodeAck { node, status, summary, approved,
                                  needs_revision, payload }
  state.vars[task_id] = payload (the typed output; summary string otherwise)
```

The same recording happens in `apply_match` (payload = the matched output
line) and in `rule(...)` when the manager marks a node done (payload = the
manager's decision JSON).

### 1.3 Template rendering over state

Replace the string-only `render_start(id, vars)` with:

```rust
/// Render a node's `start_template` against the workflow state.
/// `{key}`          → state.vars["key"]
/// `{node.summary}` → state.acks["node"].summary
/// `{node.payload}` → state.acks["node"].payload
/// `{node.payload.path.to.x}` → nested field access
/// Unresolved references stay literal; the caller logs them.
#[must_use]
pub fn render_template(template: &str, state: &WorkflowState) -> String;
```

**Rendering rule (G2):** a resolved value that is a **JSON string renders
bare** (no quotes) — this keeps the shipped graphs' prompts byte-identical
(`{feature}` → `auth`, not `"auth"`). Any other JSON value renders compact
(`{"a":1}`). This rule applies to every reference form (`{key}`,
`{node.summary}`, `{node.payload…}`).

`Workflow::render_start(&self, id)` renders the node's template against
`self.state`. The runner's per-task `vars` map disappears; `start_graph` takes
`WorkflowState` (initial vars) instead of `BTreeMap<String, String>` — see §3
(M3 interplay).

### 1.4 Tests

- `record` writes both `acks` and `vars[task_id]`.
- `{node.summary}` / `{node.payload.x}` resolve; unknown refs stay literal.
- Legacy `{feature}`-style vars still render (state.vars).
- **G2 regression:** a string var renders bare — the shipped
  `feature_lifecycle`/`bug_flow` templates produce byte-identical output to
  today.

---

## 2. P2 — Step-level journal records, runlog, rewind

**Lesson from LangGraph.** Every super-step checkpoints; state can be
inspected/replayed/forked. We persist node-state rows but no per-step snapshot
of the data state.

### 2.1 Journal record

New journal type in `supervisor-core/src/journal.rs`:

```
JournalType::WorkflowStep      // "workflow.step"
payload = {
  "ws": "…", "graph": "…", "node": "…",
  "from": "ready", "to": "running",
  "state": <WorkflowState snapshot AFTER the transition>,
  "ts": "…"
}
```

- `Fleet::record_workflow_step(ws, graph, node, from, to, state)` (journal +
  projection: **reuse the `journal` table only — no new `workflow_step` table**
  (G9). Runlog queries journal rows of type `workflow.step` for the graph.)
- `WorkflowRunner` writes it on **every** transition it persists
  (`persist_node` + the readiness pushes), with the post-transition state.
- Replay arm: `FleetState::apply` handles `workflow.step` (store in a
  `VecDeque` ring, capped at e.g. 10k steps, for the runlog accessor).

### 2.1a Journal growth bound (G4)

Append-only steps with no retention would grow the journal and slow restore
forever. Compaction rule:

- New record `workflow.run_done`:
  `{ "ws", "graph", "final_state": WorkflowState, "node_states": {...}, "instances": [...], "ts" }`
  — written once when a run completes (`RunCompleted`).
- **Compaction pass** (daily + on graceful shutdown): rewrite `journal.jsonl`
  atomically (temp + rename, mirroring the bus log prune), replacing **all
  `workflow.step` records of a completed run** with that run's single
  `workflow.run_done` record. Steps of in-flight runs are retained.
- **Restore** (§3) therefore replays: `workflow.start` → (`workflow.run_done`
  if compacted | `workflow.step` records if the run is in flight). After
  compaction, a completed run restores from one record; the journal stays
  bounded by (number of runs × 1 record) + in-flight steps.
- The in-memory runlog ring is capped (10k) as above; the runlog endpoint
  reads whatever steps remain un-compacted (recent history) and falls back to
  `run_done` for older runs.

### 2.2 Runlog + rewind surface

- `GET /api/v1/workspaces/{ws}/graphs/{g}/runlog?limit=` → the step records
  (chronological). Feeds the web UI timeline ("what just happened, in order").
- `Workflow::rewind(&mut self, node) -> Vec<WorkflowEvent>`:
  1. `node` and every transitive downstream node (via `reachable_from`) are
     reset: `Done/Running/Failed/Blocked/NeedsDecision → Pending`;
  2. `node` itself → `Ready`;
  3. **stale outputs are cleared (G6):** `state.acks` and `state.vars` entries
     owned by the reset nodes are removed, so the re-run reads fresh state
     rather than the previous attempt's output;
  4. publish a **dedicated `WorkflowEvent::Rewound { graph, node }`** (G7 —
     not the human-gate `LoopBack` event, which carries `revision` semantics)
     + `NodeReady` for the target.
  Deliberately the **fork** semantics, not full state restore: the accumulated
  `WorkflowState` is kept (the re-run sees what came before), matching our
  honest-DAG version of time travel. (Full arbitrary-checkpoint restore is a
  non-goal, §13.)
- `POST /api/v1/workspaces/{ws}/graphs/{g}/nodes/{n}/rewind` + CLI
  `supervisor dag rewind <ws> <graph> <node>`.
- Rewind is refused while a node is `Running` on the instance (400) — rewind a
  stalled run, not a live turn.

### 2.3 Tests

- Every transition produces a `workflow.step` record with the post state.
- `rewind` resets node + downstream, re-readies the node, **clears the reset
  nodes' acks/vars (G6)**, emits `Rewound` (not `LoopBack`), and keeps the
  rest of the state.
- Runlog endpoint returns steps in order; replay survives restart.
- **Compaction (G4):** after a run completes, compaction replaces its steps
  with one `run_done`; a restart after compaction restores the same node
  states and `WorkflowState` as a replay of the uncompacted steps would.

---

## 3. Interplay with M3 (restart restore, from the wiring-fixes design)

M3's `workflow.start` record changes to carry the full initial
`WorkflowState` instead of `BTreeMap<String, String>` vars:

```
payload = { "ws": "…", "graph": "…", "state": <WorkflowState> }
```

`WorkflowRunner::restore()` becomes: rebuild instance → re-apply the step
records (or the compacted `run_done`, §2.1a) for `(ws, graph)` in order (which
restores node states **and** the data state) → map `Running → Ready` → publish
readiness. The step records make restore a replay, not a reconstruction.
In-flight runs restore from steps; completed runs restore from `run_done`.
**M3 must land with or after P2.**

---

## 4. P3 — First-class interrupt + resume-with-value

**Lesson from LangGraph.** `interrupt()` pauses a node mid-execution;
`Command(resume=value)` continues with a payload. Ours pauses only at the
human gate, via an ACK loop.

### 4.1 Core

- `NodeState` gains `WaitingInput` (serde snake_case `waiting_input`).
- `DoneWhen` gains:

  ```rust
  /// A node whose completion requires an external value. **An `input` IS a
  /// completion criterion** (G5): `has_criterion()` counts it, so an
  /// interruptible node loads without `ack`/`match`. While the owning agent
  /// signals needs_input (or the human requests it), the node parks in
  /// `WaitingInput`; `resume(value)` completes it directly.
  pub input: Option<InputSpec>,          // InputSpec { prompt: String }
  ```

- `Workflow::needs_input(&mut self, node) -> Vec<WorkflowEvent>`: a `Running`
  node with `done_when.input` moves to `WaitingInput`, publishes
  `WorkflowEvent::NodeWaiting { graph, node }` (new event).
- `Workflow::resume(&mut self, node, value) -> Vec<WorkflowEvent>`: a
  `WaitingInput` node stores the value (`state.vars[node] = value`, bare-string
  rendering rules apply downstream) and **completes directly** → `Done`,
  publishes `NodeDone`. There is no re-evaluation of `ack`/`match` — those
  cannot be satisfied by a value; the input is its own completion mechanism.
  A node may carry `input` *alongside* `ack`/`match` (the agent-side path);
  whichever fires first wins.

### 4.2 Runner + API

- The runner reacts to `Signal::NeedsInput` for an agent with a `running_task`
  whose node is interruptible → `needs_input(node)`.
- `POST /api/v1/workspaces/{ws}/graphs/{g}/nodes/{n}/resume` `{ "value": … }`;
  CLI `supervisor dag resume <ws> <graph> <node> <value>`.
- `Action::ResumeNode { ws, graph, node, value }` (core `Action`) so rules and
  the manager can resume; routed via the command dispatcher (wiring-fixes F4).

### 4.3 Tests

- `needs_input` parks the node; `resume` stores the value and **completes the
  node directly** (G5); a node with only `input` (no `ack`/`match`) loads.
- Rule action `ResumeNode` routes end to end.

---

## 5. P6 — Conditional routes (general edges over state)

**Lesson from LangGraph.** Conditional edges: a function of state chooses the
next node. Our `done_when`/`on_error`/`loop_back` cover completion, failure,
and revision only.

### 5.1 Core

`DoneWhen` gains:

```rust
/// After completion, route to extra targets based on the workflow state.
/// Evaluated in order; first match wins. A route marks its target `Ready`
/// **regardless of `depends_on`** (an explicit override). The target must
/// exist (validated at graph load).
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub then: Vec<Route>,
```

```rust
pub struct Route {
    pub when: Option<Predicate>,      // None = fallback (always matches)
    pub to: String,
}
pub struct Predicate {
    pub var: String,                  // "dev.summary", "intake.items", …
    pub op: PredicateOp,
    pub value: serde_json::Value,
}
pub enum PredicateOp { Eq, Ne, Contains, In, Gte, Lte }
```

- `var` resolves against the workflow state with the same path syntax as
  templates (§1.3); unresolvable `var` → the route does not match (logged).
- On node completion, `apply_ack`/`apply_match`/`rule` evaluate `then` and
  emit `NodeReady` for matched targets (in addition to normal
  `depends_on`-driven readiness).
- **`loop_back` stays for backward compatibility**; v2 graphs may express
  revision routing as `then` routes. Documented, not removed.

### 5.2 Validation

`Workflow::new`: every route target exists; `PredicateOp::In` requires an
array `value`. (Reuses the existing validation pass.)

### 5.3 Tests

- Fallback route always fires; ordered predicates pick the first match;
  unresolvable `var` skips; routed target readies despite unsatisfied
  `depends_on`.

---

## 6. P5 — Subgraph nodes

**Lesson from LangGraph.** A compiled graph is itself a node; composition
enables reuse.

### 6.1 Core

`NodeDef` gains:

```rust
/// Run another installed graph as this node. When set, `role` may be omitted;
/// completion = the subgraph's terminal completion, failure = any terminal
/// failure. Mutually exclusive with `fan_out` (§7).
pub subgraph: Option<String>,
```

- `Workflow::new` validation: `subgraph` ids are strings (existence in the
  fleet is a runtime check); a subgraph node's `done_when` defaults to
  "subgraph complete" (any `done_when` set on it is rejected at load).

### 6.2 Runner

- `WorkflowRunner` instance key becomes `(ws, graph)` where a subgraph
  instance is keyed `(ws, "<parent_graph>#<parent_node>")` — the `graph` field
  in emitted events stays the *parent* graph id for routing, plus a new
  `scope` field on `WorkflowEvent` (or the event carries the subgraph id).
  Simplest: events carry `graph: <subgraph_id>` and the runner maps them back
  to the parent node; the parent node state is `Running` while the subgraph
  runs.
- When a subgraph node becomes `Ready`: load the subgraph from the fleet,
  start it with `state.vars` as its inputs; forward its terminal completion to
  the parent (parent node → `Done`/`Failed`), merge the subgraph's `acks` into
  the parent state under `state.vars[parent_node]` (a map of the subgraph's
  acks).
- `timeout_secs`/`on_error` on the subgraph node wrap the whole subgraph run.
- Recurrence guard: a subgraph may not (transitively) contain itself — checked
  at start (cycle in the subgraph reference graph), logged, node → `Failed`.

### 6.3 Tests

- A two-node subgraph completes → parent node done, acks merged; subgraph
  failure → parent `on_error`; self-reference rejected.

---

## 7. P4 — Runtime fan-out (`Send`-like)

**Lesson from LangGraph.** `Send` instantiates node copies at runtime (one per
item). This is our "spawn a subagent per issue/file" primitive. **Sequenced
last**: it depends on P2's step records and must coordinate with M3 restore.

### 7.1 Core

`NodeDef` gains:

```rust
/// On the node's **dispatch ack** (see below), read `state.vars[over]` (must
/// be a JSON array) and instantiate the `child` node once per item as
/// `<child>#<index>`. The fan-out node then enters a join: it completes when
/// every child completes. A child's own `on_error` applies per instance; an
/// exhausted child fails the parent (→ `needs_decision`).
pub fan_out: Option<FanOut>,
```

```rust
pub struct FanOut {
    pub over: String,                 // path into state.vars, must be an array
    pub child: String,                // template node id
    pub max_concurrency: Option<u32>, // None = unbounded
}
```

**Completion mechanism (G3 — one mechanism, no contradiction):** a `fan_out`
node **must** set `done_when.ack` — that ack is the **dispatch trigger**, and
it is what satisfies `Workflow::new`'s criterion check. On that ack:

1. record the ack normally (§1.2);
2. run `dispatch()` (instantiate children, each → `Ready`);
3. the node does **not** go `Done` on its own ack — it enters the **join**
   (stays `Running`) until every child completes, then:
   - all children `Done` → parent `Done`;
   - any child exhausted past bounds → parent `NeedsDecision` (delegate).

So "ignored" never appears: the ack is the dispatch trigger, and the join
replaces the normal ack-completion path for the parent. Validation: `fan_out`
without `done_when.ack` is rejected at graph load.

- `Workflow` gains a dynamic instance registry: `instances: BTreeMap<String, NodeDef>`
  (ids like `triage#0`). `state(node)` / `node(id)` / `states()` consult
  static nodes + instances.
- `Workflow::dispatch(&mut self, parent, items) -> Vec<WorkflowEvent>`:
  validates `over` is an array (else parent → `NeedsDecision` with reason),
  creates child instances, sets each `Ready`, publishes `NodeInstantiated`
  (new event: `{ graph, parent, child, index }`) + `NodeReady` per child.

### 7.2 Runner + persistence

- `dispatch` runs on the parent's dispatch ack (inside `apply_ack` on the
  parent). Child ACKs match by their instance task ids (`child#i`).
- **Parallelism is roster-bounded (G8):** children resolve roles like any
  node; the effective concurrency is `min(max_concurrency, available agents
  with the child's role)`. Queued children stay `Ready` and start as agents
  free up (normal inbox delivery). `max_concurrency` limits *dispatched*
  children, not physical agents.
- Persistence: each `NodeInstantiated` + child transition writes a
  `workflow.step` record (P2); `workflow.run_done` includes the instance
  registry (`instances: [child ids]`). **M3 restore** replays `NodeInstantiated`
  (or the `run_done` registry) to rebuild instances before re-applying states.
  This is why P4 is last and only after M3 is proven live.

### 7.3 Tests

- `dispatch` creates N instances from the array; children resolve roles and
  complete independently; parent joins (all-done → done; failure → decision);
  empty/missing array → `NeedsDecision` with reason; restore replays instances.
- **G3 regression:** a `fan_out` node without `done_when.ack` is rejected at
  load; with it, the dispatch ack does not complete the parent (join holds it
  `Running`).
- **G8:** with one agent for the child's role, children run one at a time
  regardless of `max_concurrency`.

---

## 8. P7 — Transition streaming

**Lesson from LangGraph.** Consumers stream per-step updates; ours streams
coarse events, so UIs must poll.

### 8.1 Core + runner

New core event:

```rust
WorkflowEvent::NodeTransition {
    graph: String, node: String,
    from: NodeState, to: NodeState,
    /// The workflow-state delta for this transition (keys that changed).
    state_delta: serde_json::Value,
}
```

plus `RunStarted { graph }` / `RunCompleted { graph }`.

- The runner publishes `NodeTransition` on **every** `persist_node` call (from
  the previous state to the new one) and on readiness pushes; `state_delta`
  carries the changed `vars`/`acks` keys with values (a small diff, not the
  whole state).
- Consumers: the web UI's live canvases + timeline subscribe to the existing
  `/api/v1/events` SSE and render per-transition diffs with zero polling.

### 8.2 Tests

- Every transition emits exactly one `NodeTransition` with the right
  from/to and a delta containing only changed keys.

---

## 9. Event/journal/API surface additions (summary)

| Kind | Additions |
|---|---|
| Events (`WorkflowEvent`) | `NodeWaiting`, `NodeInstantiated`, `NodeTransition`, `Rewound`, `RunStarted`, `RunCompleted` |
| Journal types | `workflow.step` (P2), `workflow.run_done` + compaction pass (G4); `workflow.start` payload upgraded to carry `WorkflowState` (M3 interplay) |
| API | `GET …/graphs/{g}/runlog`; `POST …/graphs/{g}/nodes/{n}/rewind`; `POST …/graphs/{g}/nodes/{n}/resume` |
| CLI | `supervisor dag rewind <ws> <graph> <node>`; `supervisor dag resume <ws> <graph> <node> <value>` |
| Core `Action` | `ResumeNode { ws, graph, node, value }` (rules/manager can resume) |
| `NodeDef` new fields (all `#[serde(default)]`) | `done_when.input` (P3), `done_when.then` (P6), `subgraph` (P5), `fan_out` (P4) |

---

## 10. Phasing and dependencies (recap)

**Phase A2 — now, with the wiring fixes.** P1 → P2 → P7 (in that order; P2
depends on P1's `WorkflowState`, P7 is trivial once P2 exists). M3 (restart
restore) is implemented against P2's replay, not against raw node rows.

**Phase B2 — after web UI v1.** P3 → P6 → P5 → P4. P3 is small and unblocks
human-in-the-loop polish; P6 generalizes routing; P5 adds composition; P4 last
(depends on P2 + M3 proving restore, and is the largest change to the runner's
instance model).

**Gates:**

- Phase A2 done = P1/P2/P7 tests green + the live chain smoke (wiring-fixes
  §verification) passes *with* `workflow.step` records in the journal.
- Phase B2 start = web UI v1 (U1–U6) shipped.

---

## 11. DAG editor schema note

The web-UI DAG editor must not freeze the node schema before P3–P6 fields
exist. Concretely: the editor's `NodeDef` type and property panel are built
from the core's serialized shape (a shared JSON schema exported by
`supervisor-core`), **including** the new optional fields — so phase B2 adds
editor panels, not schema migrations. Add this requirement to the web-UI
design's §4 (editor) as a must.

---

## 12. Testing strategy

- **Core (pure, unit):** every P has its table in §1–§8. The golden rule:
  every transition function (`apply_ack`, `apply_match`, `rule`, `rewind`,
  `needs_input`, `resume`, `dispatch`) is total — returns events, never
  panics, and leaves the instance in a legal state (asserted by a
  `Workflow::invariant()` helper: no unknown node in a state, no
  `Running` node without a criterion, route/fan-out/subgraph references valid).
- **Runner (integration):** each P against a real daemon + fake drivers:
  templates render state; step records appear in the journal; rewind re-runs a
  node; resume completes a `waiting_input` node; a routed target readies
  without `depends_on`; a subgraph runs and merges acks; fan-out instantiates
  N children and joins; restart replays a fan-out mid-run (P4+M3).
- **Web UI:** the transition stream renders diffs (P7) and the runlog timeline
  (P2) without polling.

---

## 13. Non-goals (deliberate LangGraph divergences)

- **Cycles/recursion limits**: general graph cycles stay rejected; `loop_back`
  (and P6 routes) express the bounded rework case.
- **Full checkpoint restore / arbitrary time travel**: P2 gives step records +
  rewind (fork). Restoring an arbitrary historical checkpoint into a live
  instance is out of scope — that is LangGraph's thread/checkpointer model,
  not our journal model.
- **In-process node execution, Python/LangChain stack, super-step batching**:
  nodes remain external agent turns; the engine stays pure Rust, event-driven.
- **A general `Store` (long-term memory)**: decision log + rules remain the
  memory; a LangGraph-style KV store is a separate decision if it ever comes.

---

## 14. Decisions (resolved)

1. **P1 state model**: `WorkflowState { vars, acks }` — vars are inputs +
   node outputs; acks are the provenance. No per-field reducers (a
   last-write-wins merge per key; conflict semantics belong to nodes, not the
   engine). Accessor is `workflow_state()` (G1).
2. **Rendering rule (G2)**: resolved JSON strings render **bare**; all other
   JSON renders compact. Shipped graphs stay byte-identical.
3. **Rewind = fork, not restore** (P2): reset node+downstream, **clear the
   reset nodes' acks/vars (G6)**, emit the dedicated `Rewound` event (G7),
   keep the rest of the state.
4. **Journal compaction (G4)**: completed runs collapse their `workflow.step`
   records into one `workflow.run_done` record; daily + shutdown compaction
   keeps the journal bounded and restore O(runs + in-flight steps).
5. **Fan-out completion (G3)**: `fan_out` **requires** `done_when.ack`; that
   ack is the dispatch trigger, and the join (children complete) replaces the
   normal ack-completion path. No "ignored" done_when anywhere.
6. **Interrupt completion (G5)**: `done_when.input` is itself a completion
   criterion (`has_criterion()` counts it); `resume(value)` stores the value
   and completes the node directly — no ack/match re-evaluation.
7. **`loop_back` kept** alongside P6 routes (backward compatibility).
8. **Subgraph failure = parent `on_error`**; subgraph acks merge under the
   parent node's key.
9. **Fan-out join = parent completes when children complete**; parent failure
   on any exhausted child; parallelism roster-bounded (G8).
10. **P4 gated on M3** — instance replay is the hard part of restore.
11. **Node schema additions are serde-defaulted and editor-schema-safe** (§11).
