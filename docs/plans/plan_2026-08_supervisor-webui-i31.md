# Supervisor web UI — I-31 live surface — detailed software design

> **Kind:** detailed software design (implementation-ready). Follows the
> approved high-level plan
> [`plan_high_level_2026-08_supervisor-webui-i31.md`](plan_high_level_2026-08_supervisor-webui-i31.md).
> **Status:** designed. Ledger: `docs/ledger.md` →
> `plan_2026-08_supervisor-webui-i31`.
> **System hub:** `docs/specs/2026-08-14-supervisor-webui-detailed-design.md`,
> `docs/specs/2026-08-13-supervisor-detailed-design.md`
>
> Last updated: 2026-08-15.

## 0. Locked product decisions (high-level open questions, resolved)

1. **Default dashboard tab = Live.** Live is the product; Stats is second.
2. **Triage = pinned strip at the top of the Live tab** + count badge on the
   tab button. Not a nav page.
3. **fg/bg filter = per-workspace-card segmented control on the Live tab**
   (filters that card's agent rows) + the same control on the workspace detail
   page. Not one global control.
4. **Adopted cmux workspace on `off()` = treat adopted as owned.** Close it
   like any supervisor-created workspace (matches I-6's recorded-pid intent).
5. **Functionality first:** Phase A (daemon + CLI) is complete and CLI-verified
   before Phase B (web UI) starts.
6. **Decide banner Depth 2:** context + Done/Rerun/Skip via one new endpoint.
7. **Live cards are agents-first:** one agent row per agent; a canvas renders
   only while a workflow runs.
8. **SSE replaces the 2s node-state poll.** `workspace_id` is added at the bus
   boundary (not the engine).
9. **A decide ruling is recorded as a `DecisionRecord`** (journal-first via
   `Fleet::append_decision`), so it lands in the decision log and feeds
   bake-back.

## 1. Scope

### In scope

- Phase A (daemon + CLI): A1 `workspace_id` on bus workflow events; A2
  `missing_role` surface state + recheck; A3 cmux adopt-or-create; A4 decide
  endpoint + `supervisor dag decide`; A5 triage endpoint + `supervisor status`
  triage section.
- Phase B (web UI): B1 reducer/types; B2 canvas upgrades + SSE; B3 dashboard
  Live/Stats tabs; B4 workspace detail page; B5 agent dialog activity feed +
  decide banner; B6 intake page, rules page, full editor property panel.

### Out of scope

Playwright e2e + a11y pass (polish plan). Graph engine v2 P3–P7. Spawning new
agents into a workspace (no capability exists; record the deferral in the
web-UI spec). Decide Depth 3. agent-bus bridge worker (I-34, deferred).
ratatui dashboard.

### Deliverable phases (implement in order)

- **Phase A** — A1 → A2 → A3 → A4 → A5. Gate: `cargo test --workspace` +
  clippy + fmt green, and a live CLI walk of A3–A5 (no browser).
- **Phase B** — B1 → B2 → B3 → B4 → B5 → B6. Gate: `cd web && npm run test &&`
  `npm run build` green + manual walk per §11.

## 2. Config

No new `supervisor.toml` keys. No new CLI flags beyond `--action`/`--reason` on
`dag decide`. The triage and decide endpoints need no configuration.

## 3. Data model

### 3.1 NodeState gains a surface-only `missing_role` value

- Add `MissingRole` to `supervisor_core::types::NodeState` with a doc comment:
  **surface marker only — the engine never sets it.** The engine holds the node
  at `Ready` when the role is absent (`dag.rs:462-464`); the daemon persists
  `MissingRole` so triage/canvas can show the hold. Any later transition
  (`NodeReady`/`NodeStarted`) overwrites the row — that is the clear path.
- `db.rs` codec (`db.rs:1139-1161`): add `"missing_role"` to `to_db` and
  `from_db` (unknown strings still fall back to `Pending` — replay-safe).
- Web `types.ts`: add `"missing_role"` to the `NodeState` union; reducer arm.

### 3.2 Workflow events on the bus gain `workspace_id` at the boundary

- `event.rs`: change `BusEvent::Workflow(WorkflowEvent)` to
  `BusEvent::Workflow { workspace_id: String, event: WorkflowEvent }`.
  The engine's `WorkflowEvent` stays untouched (the engine has no workspace).
- Wire shape (serde tag `topic` stays `"workflow"`):
  `{"topic":"workflow","workspace_id":"iot","event":{"event":"node_done","graph":"bug_flow","node":"fix"}}`.
- **Journal is unaffected:** the journal stores `WorkflowTransitionEvent`
  (`journal.rs:232`), not `BusEvent`; its golden wire-compat test must keep
  passing. Publish sites in `services/workflow.rs` attach the workspace they
  already know.
- The web is the only consumer of this shape; it updates in the same plan
  (B1). An old web build against a new daemon no-ops safely (unknown nested
  shape → reducer ignores).

### 3.3 Ruling records (no schema change)

- A decide ruling is a `DecisionRecord` (`types.rs:304-315`):
  - `signature` = `"human.ruling.<graph>/<node>"` (per-node cluster; needs ≥3
    occurrences to bake, so it stays quiet),
  - `situation` = `{"ws","graph","node","state":"needs_decision","reason"}`,
  - `decision` = `{"action":"done|rerun|skip","reason","source":"human"}`,
  - `outcome` = `None` (filled later by the existing outcome observer).
- Written journal-first via `Fleet::append_decision` (`state.rs:255`) — the
  C-2 rule. Replay re-inserts via the existing `JournalType::DecisionRecord`
  arm (`state.rs:582`). No new journal type, no new table.

### 3.4 Node-state rows for `missing_role` (recheck source)

- The recheck source is the persisted rows themselves: `Fleet::node_states(ws,
  graph)` filtered to `state == MissingRole`. No new table.

## 4. Db API

- `DbCodec for NodeState` (db.rs): two new arms (§3.1).
- No new tables, no migrations beyond the enum strings (stored as TEXT).
- Triage reads reuse existing iterators: `Fleet::agents(ws)` and
  `Fleet::node_states_all(graph)` / `node_states(ws, graph)` — the state
  filter lives in the handler. No new query layer.

## 5. Protocol

### 5.1 New REST endpoints (bearer-authed like every `/api/v1` route)

**`GET /api/v1/triage`** — read-only aggregate; no journal. Response:

```json
{
  "agents": [ { "ws": "iot", "agent_id": "dev_01", "state": "blocked_permission",
                "permission_id": null } ],
  "nodes":  [ { "ws": "iot", "graph_id": "bug_flow", "node_id": "fix",
                "state": "needs_decision", "error": null } ]
}
```

- Agents filtered to `waiting_input` / `blocked_permission` / `error`.
- Nodes filtered to `needs_decision` / `failed` / `blocked` / `missing_role`.
- Sorting and filtering are client-side (the endpoint stays dumb).

**`POST /api/v1/workspaces/{ws}/graphs/{graph}/nodes/{node}/decide`**

- Body: `{ "action": "done" | "rerun" | "skip", "reason": "…" }`.
- 200 → `{ "node": "<id>", "state": "<new state>", "action": "…" }`.
- 409 → the node is not in `needs_decision` (or no instance exists).
- 404 → unknown workspace/graph/node.
- Handler order (journal-first): validate → `append_decision` (ruling record)
  → engine `ruling()` → apply the returned events exactly like the
  `apply_ack` path (`services/workflow.rs:495+` pattern): persist transitions,
  publish on the bus, clear running-task bookkeeping.

### 5.2 CLI

- `supervisor dag decide <graph> <node> --action done|rerun|skip --reason "…"`
  prints the ruling result. Unknown graph/node → exit 2 (I-20 pattern);
  not-needs-decision → exit 1 with the message.
- `supervisor status` gains a triage section after the agent rows: one line
  per attention-state node (`graph/node state ws`), or
  "triage: nothing needs attention".

### 5.3 SSE

- `BusEvent::Workflow` shape per §3.2. Nothing else changes on the stream.

## 6. Domain modules

### `supervisor-core`

- `types.rs`: `NodeState::MissingRole` (+ codec strings in the daemon).
- `event.rs`: `BusEvent::Workflow { workspace_id, event }` + roundtrip tests.

### `supervisor-daemon`

- **`services/workflow.rs`**
  - Every `BusEvent::Workflow` publish attaches the workspace.
  - On `WorkflowEvent::MissingRole` → `persist_node(ws, graph, node,
    NodeState::MissingRole)` (existing `persist_node` pattern,
    `workflow.rs:594-618`). Clear-on-transition is automatic: the next
    `NodeReady`/`NodeStarted` persist overwrites.
  - `recheck_missing(ws)`: for each row in `missing_role` for that workspace,
    call the existing `on_ready(ws, graph, node)` — the held node re-resolves
    its role; if an agent now exists, delivery proceeds; if still absent, the
    hold stays. Triggers: `FleetEvent::AgentState` where the new state is
    `Idle` or `Working` (a session exists), and workspace `on`.
  - `decide(ws, graph, node, action, reason) -> Result<NodeState>`: look up the
    instance, verify the node's state is `NeedsDecision` (engine truth), call
    `Workflow::ruling()` (`dag.rs:615`), apply events as in `apply_ack`.
- **`services/workspace.rs`** — adopt-or-create in `on()` (`workspace.rs:250`):
  1. `cmux.list_workspaces()` (`clients/cmux.rs:31`); match by the
     deterministic workspace name (the id passed to `new_workspace` today).
  2. Match found → adopt: record `cmux_ws` + its surfaces as recorded handles
     (same bookkeeping as created panes); do NOT call `new_workspace`.
  3. No match → create as today.
  4. `ensure_panes` still fills any missing foreground panes (adopt + fill).
  5. Journal the resulting `WorkspaceState` (existing upsert path) so the
     adopted `cmux_ws` survives restart.
  - `off()` closes adopted workspaces too (locked decision 4) — after
    adoption the supervisor owns them.
- **`api.rs`** — two new handlers (§5.1) in the existing router table;
  `route_layer` auth already covers them (`api.rs:70-100`).
- **`db.rs`** — codec arms only.

### `supervisor-cli`

- `DagAction::Decide { graph, node, action, reason }` + client call.
- `status` prints the triage section from `GET /api/v1/triage`.

## 7. Web surface (Phase B, in `web/`)

### 7.1 Reducer + types (B1)

- `types.ts`: `NodeState` += `"missing_role"`; the `BusEvent` workflow arm
  becomes `{ topic: "workflow"; workspace_id: string; event: { event: string;
  graph: string; node?: string; … } }`.
- `reduce.ts`: `workflowNode` reads `e.event`; key node states under
  `nodeStates[workspace_id][graph][node]` — delete the synthetic `""` fallback
  (`reduce.ts:77-89`). Add the `missing_role` arm. `lastEvents` stays.
- `live-store.tsx` unchanged (abortable pump already landed, `sse.ts:73`).

### 7.2 Canvas upgrades + SSE (B2)

- `WorkflowCanvas.tsx` (`toFlow` + `StateCard`):
  - State glyph beside the role glyph: done ✓, failed ✕, blocked ⛔,
    needs_decision ! (slow pulse), missing_role ⚠; `aria-label` carries the
    state name (a11y: state never color-only).
  - `loop_back` edges: for each node with `loop_back {on?, small, big}`,
    dashed violet edges gate→target with labels "small"/"big" ("on"
    unlabeled); animated only while a `LoopBack` event is in flight (a few
    seconds).
  - `on_error` tag: text chip — "on_error: delegate | skip | rerun ×N".
  - New `idle?: boolean` prop: last-run states at low emphasis, no spinner,
    caption "idle — last run <time>", still clickable. Not a new mode.
- Poll removal: delete the `refetchInterval: 2000` node-state queries
  (`Dashboard.tsx:20-30`, `Graphs.tsx:10-20`). Initial state loads once from
  `GET /graphs/{id}/nodes?ws=`; updates arrive over SSE from the live store.
  The reducer is the single state authority afterwards.

### 7.3 Dashboard Live/Stats tabs (B3)

- `Dashboard.tsx` becomes a tab shell: **Live** (default) | **Stats**;
  component state, no URL change. The Live tab button carries the triage
  count badge.
- **Live:**
  - Triage strip (pinned top, per high-level R5): rows from
    `GET /api/v1/triage` on mount, then SSE updates; sorted client-side by
    severity (blocked_permission → waiting_input → needs_decision → error →
    failed → blocked → missing_role). Each row: glyph + label + ws; click →
    agent dialog (agent rows) or `#/graphs/<id>` (node rows). Empty state:
    "nothing needs attention".
  - Workspace card (one per running workspace): header (name, state badge,
    on/off), fg/bg segmented control (filters this card's agent rows), agent
    rows (agent chip + state + queue depth; click → agent dialog; actions:
    message, attach), a live canvas section only while a workflow runs, and a
    "start workflow" action (installed-graph picker → existing start
    endpoint).
  - Collapsed "off workspaces" section: name + state + an `on` button each.
  - Header action: **resume** (`POST /api/v1/resume`).
- **Stats:** the metrics strip (existing) + `time_series` mini-chart
  (hand-rolled SVG bars, 1h buckets) + per-workspace/per-agent tables from
  `GET /api/v1/metrics` + shortcut links (Graphs, Rules, Decisions, Intake).

### 7.4 Workspace detail page (B4)

- Route `#/workspaces/:ws` renders the real page (no longer the filtered
  Dashboard — `app.tsx:64`).
- Sections: controls (on/off graceful/resume) with error surfacing
  (I-28 pattern); agent grid with the fg/bg segmented control; per-agent 24h
  cost mini-chart (hand-rolled SVG bars from `GET /api/v1/usage?ws=&agent=&
  since=`, buckets of 1h; cost in cents, "est." label); installed-graph
  canvases (running = live; idle = `idle` prop with last-run states).

### 7.5 Agent dialog: activity feed + decide banner (B5)

- **Activity feed** (spec §5.4): a horizontal timeline strip above the
  transcript — ticks from `live.lastEvents` filtered by (ws, agent):
  step_started/step_ended/step_failed, tool_failed, diff, permission_asked,
  needs_input, session_error, session_status. Last ~10 with time + glyph +
  label; older in a hover/expand. `role="log"` `aria-live="polite"`.
- **Decide banner** (Depth 2): rendered when the agent is `error` or one of
  its nodes is `needs_decision`. Amber strip (permission-banner pattern):
  "`<node>` in `<graph>` needs a decision — <reason>" + buttons **Done /
  Rerun / Skip** → `POST …/nodes/{node}/decide` (I-28-style error surfacing
  on failure). Also: a matching action row in triage, and the canvas ! badge
  routes here.

### 7.6 Absorbed surfaces (B6)

- **Intake page** (`/intake`): `GET /api/v1/intake` rows — kind, title,
  severity, `graph_id` link, `received_at`. Nav entry.
- **Rules page** (`/rules`): `GET /api/v1/rules` list; TOML textarea add
  (`POST /api/v1/rules`); reload button (`POST /rules/reload`). Nav entry.
- **Editor property panel** (full §5.3 set, `Graphs.tsx:104-128` today):
  role, agent_id, start_template, done_when.ack / done_when.approved /
  done_when.match, on_error (select: delegate / skip / rerun + max input),
  gate, loop_back small/big (+ on), mode, timeout_secs. All edits through
  the pure `lib/graph-edit.ts` helpers; validation mirrors core (the
  graph-engine v2 schema note: no new fields frozen here, `2026-08-14-supervisor-graph-engine-v2.md:524`).

## 8. Testing matrix

**Phase A (cargo):**

- `event.rs`: new `BusEvent::Workflow` roundtrip (JSON → enum → JSON); the
  journal golden test for `WorkflowTransitionEvent` stays untouched and
  green.
- `db.rs`: `missing_role` codec roundtrip both directions; unknown string →
  `Pending` unchanged.
- `workflow.rs`: `MissingRole` event → row persisted; agent appears (AgentState
  Idle) → recheck → delivered → row becomes `running` (clear-on-transition);
  recheck with no agent → hold stays; `decide`: done/rerun/skip transitions +
  `DecisionRecord` journaled; 409 path (not needs_decision); double-decide is
  a no-op.
- `workspace.rs`: `on()` twice → exactly one cmux workspace (adopt); adoption
  records handles; missing foreground panes filled; `off()` closes adopted.
- `api.rs`: triage + decide route shapes, auth required, 404/409.
- `cli`: `dag decide` arg parsing + exit codes; `status` triage section.

**Phase B (vitest + RTL):**

- `reduce.ts`: nested workflow event keys `(ws, graph, node)`; `missing_role`
  arm; `loop_back` revert behavior.
- `WorkflowCanvas`: glyph per state; loop_back edge count/labels; on_error
  tag; idle prop rendering.
- Dashboard: tabs render; agent rows; canvas only while running; off section;
  triage strip sorting.
- Workspace page: fg/bg filter; 24h chart buckets.
- Agent dialog: feed ticks; decide banner buttons hit the endpoint.
- Intake/Rules pages; property panel field edits → graph JSON.

**E2E (Playwright):** out of scope — the polish plan.

## 9. Implementation order (coding agent checklist)

Phase A, in order, `cargo test --workspace` + clippy + fmt after each:

1. A1 `event.rs` wrapper + publish sites + tests.
2. A2 core `NodeState::MissingRole` + db codec + persist on `MissingRole` +
   `recheck_missing` + triggers + tests.
3. A3 cmux adopt-or-create + adopt bookkeeping + tests.
4. A4 decide: `Workflow::ruling()` call path in the workflow service +
   `append_decision` record + route + `dag decide` + tests.
5. A5 triage route + `status` section + tests. Phase A gate: live CLI walk.

Phase B, in order, `cd web && npm run test && npm run build` after each:

6. B1 types + reducer + tests.
7. B2 canvas glyphs/edges/tag/idle + poll removal + tests.
8. B3 dashboard tabs + triage strip + workspace cards + tests.
9. B4 workspace detail page + tests.
10. B5 activity feed + decide banner + tests.
11. B6 intake, rules, property panel + tests.

Then: update the web-UI spec (polling note → SSE; new endpoints in the API
table; "start new agents" deferral record), the supervisor spec (§13-style
records for the decide/triage/adopt additions), the polish plan scope trim,
and `docs/ledger.md`.

## 10. Forbidden

- No new polling for node states — the 2s poll must not be replaced by any
  other poll.
- No chart library dependency (hand-rolled SVG only).
- No engine-semantics change for `missing_role` — surface marker only; the
  engine keeps holding at `Ready`.
- Never write the DB without the journal entry first (decide → append_decision
  before transitions; C-2 rule).
- Never store the token in localStorage/sessionStorage.
- No Decide Depth 3, no agent spawning, no graph-schema freeze.
- Keep the `WorkflowTransitionEvent` journal shape byte-compatible.

## 11. Acceptance

- Phase A: `cargo test --workspace` / clippy / fmt green; live CLI walk:
  `supervisor on` twice (one cmux workspace), a timeout parked node →
  `supervisor dag decide … --action rerun` transitions it, `supervisor status`
  shows the triage section, journal lines for the ruling + transitions.
- Phase B: `npm run test` + `npm run build` green; manual walk: Live tab shows
  running workspaces with agent rows (canvas only while a workflow runs);
  triage strip lists and links; workspace detail shows filter + cost chart +
  resume; agent dialog shows feed + decide banner; canvas shows glyphs +
  loop_back dashed edges + on_error tags; intake/rules pages and the full
  property panel work; node states update over SSE with no 2s polling
  (network tab).

## Related

- High-level: `docs/plans/plan_high_level_2026-08_supervisor-webui-i31.md`
- Review I-31 / I-26 / I-21 / I-6: `docs/reviews/review_2026-08_supervisor-v2.md`
- Web-UI spec: `docs/specs/2026-08-14-supervisor-webui-detailed-design.md`
- Supervisor spec: `docs/specs/2026-08-13-supervisor-detailed-design.md`
- Graph engine v2: `docs/specs/2026-08-14-supervisor-graph-engine-v2.md`
- Polish plan (sibling): `docs/plans/plan_high_level_2026-08_webui-polish-e2e.md`
- Product record: `PRODUCT.md`
