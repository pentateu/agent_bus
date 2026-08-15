# Supervisor web UI — I-31 live surface (the missing spec'd features) — high-level plan

> **Kind:** high-level plan (product + architecture intent). Not the detailed
> software spec yet.
> **Status:** approved (2026-08-15, Plannotator). Ledger: `docs/ledger.md` →
> `plan_high_level_2026-08_supervisor-webui-i31`.
> **Sibling:** [`plan_high_level_2026-08_webui-polish-e2e.md`](plan_high_level_2026-08_webui-polish-e2e.md) — scope reduced, see R12.
> **Product:** the supervisor web UI in `web/` (Vite + React + TS), served by
> `supervisor-daemon` at `http://127.0.0.1:4198/ui/`.
> **System hub:** [`docs/specs/2026-08-14-supervisor-webui-detailed-design.md`](../specs/2026-08-14-supervisor-webui-detailed-design.md),
> [`docs/specs/2026-08-13-supervisor-detailed-design.md`](../specs/2026-08-13-supervisor-detailed-design.md)
> **Detail software design:** [`docs/plans/plan_2026-08_supervisor-webui-i31.md`](plan_2026-08_supervisor-webui-i31.md)
>
> Last updated: 2026-08-15.

This plan builds every feature the review lists in I-31
(`docs/reviews/review_2026-08_supervisor-v2.md:206`) — the human decided
**build, not strike**. The grill session closed the product questions on
2026-08-15; the locked decisions below are the answers.

**Sequencing directive (locked): functionality first.** Phase A makes every
functional piece work and verifiable from the command line (daemon + CLI
changes only). The web UI (Phase B) starts only after Phase A is solid — the
UI is a thin client over a proven core, not a parallel build.

## Requirements (locked)

**R1. The dashboard represents what is running.** It has two tabs: **Live**
(default) and **Stats**. Live shows the running workspaces; Stats shows
metrics only.

**R2. Stats tab:** the metrics strip (today's totals), the `time_series`
mini-chart, per-workspace/per-agent tables, and shortcut links to Graphs,
Rules, Decisions, Intake. No canvases, no workspace cards.

**R3. Live tab — agents first.** One **workspace card** per **running**
workspace. The card's primary content is the agent roster: one **agent row**
per agent — a workspace has several agents, so the card holds several rows —
each row showing agent id, role, live state, queue depth. Agents work outside
predefined workflows too, and the card must show that. Clicking a row opens
that agent's dialog. A live canvas appears **only while a workflow runs** in
that workspace; when no workflow runs there is no canvas and no workflow
placeholder — just the agent rows. Card extras: workspace state badge, on/off
controls, triage badge. Quick actions: message an agent (from its row), start
a workflow (`POST /workspaces/{ws}/graphs/{graph}/start` exists,
`endpoints.ts:48`), attach. A collapsed "off workspaces" section keeps the
ability to turn a workspace on (otherwise an off workspace is invisible and
un-startable from the UI). A **resume** button in the Live header
(`POST /api/v1/resume` exists, unused).

**R4. Workspace detail page** (`#/workspaces/:ws`, today just a filtered
Dashboard — `app.tsx:64`): foreground/background agent filter (segmented
control; data from `Agent.mode`), per-agent 24h cost mini-chart
(`GET /api/v1/usage` exists), controls on/off (graceful)/resume, and a canvas
card for **every installed graph**: running = live states; idle = dimmed +
last-run states + "idle" caption. This resolves the §5.1 vs §5.2 conflict the
reviewer flagged: dashboard = running only; workspace view = installed.

**R5. Triage list** (§5.1): one global list of every agent in
`waiting_input` / `blocked_permission` / `error` and every node in
`needs_decision` / `failed` / `blocked` / `missing_role`, workspace-scoped,
clickable to the agent dialog or to the graph with the node selected. Live via
SSE. Empty state: "nothing needs attention".

**R6. Agent activity feed** (§5.4): in the agent dialog, timeline ticks from
SSE signals — `step_started/ended/failed`, `tool_failed`, `diff`,
`permission_asked`, `needs_input`, `session_error`, `session_status` — filtered
by (ws, agent) from the existing `lastEvents` ring buffer (`reduce.ts:13`).

**R7. Decide banner, Depth 2** (§5.4): when an agent is `error` or a node is
`needs_decision` (timeout / failed ACK / manager confidence < 0.5), show an
amber banner — same pattern as the permission banner (`Agent.tsx:66-72`) —
with context (node, graph, ws, reason) and buttons **Done / Rerun / Skip**
calling a new decide endpoint. Journaled like every state change.

**R8. Canvas upgrades** (§6.2-6.3): state glyphs in addition to color —
✓ done / ✕ failed / ⛔ blocked / ! needs_decision / ⚠ missing_role — with
`aria-label` (a11y: state never color-only). `loop_back` drawn as dashed
"revision" edges from the human gate back to its `small`/`big` targets,
distinct color, labeled. `on_error` shown as a small tag on the node.
Node states arrive over **SSE** — workflow events gain `workspace_id` — and
the 2s poll (`Dashboard.tsx:24`, `Graphs.tsx:16`) is removed.

**R9. Editor property panel** (absorbed from the polish plan, its §5): the
full §5.3 node field set — `agent_id`, `done_when.approved`, `done_when.match`,
`on_error`, `gate`, `loop_back` small/big, `timeout_secs` — plus the existing
role / start_template / done_when.ack / mode (`Graphs.tsx:104-128`).

**R10. Intake page + Rules page** (absorbed from the polish plan, its §2):
intake list (kind, title, severity, graph link — `GET /api/v1/intake`); rules
list + TOML textarea add + reload (`GET/POST /api/v1/rules`, `POST
/rules/reload`). Nav entries for both.

**R11. Functional additions** (Phase A, each specified in §Backend), all
drivable from the command line: `GET /api/v1/triage`; persist `missing_role`
with clear-on-transition; `workspace_id` on `WorkflowEvent` with
`#[serde(default)]` + old-row journal replay; `POST
/api/v1/workspaces/{ws}/graphs/{graph}/nodes/{node}/decide` plus a
`supervisor dag decide <graph> <node> --action done|rerun|skip --reason …`
CLI command; cmux adopt-or-create on workspace `on`; `supervisor status`
gains a triage section (attention-state nodes per workspace, alongside the
existing agent rows + queue depth).

**R12. Polish plan scope shrinks** to Playwright e2e + a11y only. Its
intake/rules/property-panel items move here; its SSE bearer fix already landed
(`web/src/api/sse.ts:73`). Both plans recorded in `docs/ledger.md`.

## Phasing (locked — functionality first)

**Phase A — functional core (daemon + CLI only).** Every functional piece
works and is verified from the terminal before any UI work starts:

- A1. `workspace_id` on `WorkflowEvent` + serde/journal compat.
- A2. `missing_role` persist + clear-on-transition.
- A3. cmux adopt-or-create on `on()` — `supervisor on` twice creates no
duplicate cmux workspace; adopted state visible in `supervisor status`.
- A4. Decide path: endpoint + `supervisor dag decide` — park a node in
`needs_decision` (timeout), rule it done/rerun/skip from the CLI, watch the
node transition and the journal record it.
- A5. Triage data: `GET /api/v1/triage` + the `supervisor status` triage
section (also closes I-21's missing queue-depth surface).

Phase A gate: `cargo test --workspace`, clippy, fmt green + a live CLI walk of
A3–A5 (no browser). Phase A is solid when a human can run the whole loop from
the terminal.

**Phase B — web UI on the solid core.** B1 reducer/types (workspace-scoped
node states, `missing_role` arm) → B2 canvas upgrades + SSE (polls removed) →
B3 dashboard Live/Stats tabs (agents-first, R3) → B4 workspace detail page →
B5 agent dialog: activity feed + decide banner → B6 absorbed pages (intake,
rules) + full property panel.

## Current vs target

| Feature | Current (verified) | Target |
|---|---|---|
| Dashboard | metrics strip + all-workspace cards, no tabs (`Dashboard.tsx`) | Live (default) / Stats tabs; workspace cards, one agent row per agent, canvas only while a workflow runs |
| Workspace view | missing — routes to filtered Dashboard (`app.tsx:64`) | detail page: fg/bg filter, cost chart, resume, installed canvases |
| Triage | one-line count per ws (`Dashboard.tsx:76,112`) | CLI: `status` triage section (Phase A); UI: global list, clickable, SSE-live (Phase B) |
| missing_role | event published, never stored (`workflow.rs:582`) | persisted + cleared on transition |
| Activity feed | signals arrive, no consumer | timeline in agent dialog |
| Decide | no path — node parks; human acts via CLI | banner + endpoint (Depth 2) |
| Canvas glyphs | color + role emoji only (`WorkflowCanvas.tsx:58-72`) | state glyphs + loop_back edges + on_error tag |
| Node-state updates | poll every 2s (`Dashboard.tsx:24`, `Graphs.tsx:16`) | SSE (workflow events carry `workspace_id`) |
| Property panel | role/ack/mode only (`Graphs.tsx:104-128`) | full §5.3 set |
| Intake / Rules pages | missing | built over existing endpoints |
| cmux on() | always `new_workspace` (`workspace.rs:250-254`) | adopt-or-create via `list_workspaces` (`clients/cmux.rs:31`) |

## Design

### IA & navigation

- Topbar: Dashboard, Graphs, Rules, Decisions, Intake.
- Dashboard = tabs **Live** (default) | **Stats**. Tab state is component
  state (no URL change). Live is the product: it represents what is running.
- Triage = a pinned strip at the top of the Live tab (bootstrap from
  `GET /api/v1/triage`, updates from SSE) + a count badge on the Live tab
  button. Empty state: "nothing needs attention".
- Component vocabulary (fixed): **workspace card** = one per running workspace;
  it contains N **agent rows** (one per agent, clickable → agent dialog) and,
  only while a workflow runs, one live canvas section. Workspace cards link to
  `#/workspaces/:ws` (detail, R4).

### Live store (reducer) changes

- `WorkflowEvent` gains `workspace_id` → `nodeStates` becomes keyed
  `(ws, graph, node)` for real; delete the synthetic `""`-key fallback
  (`reduce.ts:77-89`).
- Add `missing_role` to the `NodeState` union + a reducer arm
  (`workflowNode` default case → `missing_role` event).
- `lastEvents` stays as the activity-feed source; feeds are filtered views
  over it.

### Triage data

`GET /api/v1/triage` returns two flat lists (a dumb endpoint; sorting and
filtering are client-side):

```json
{ "agents": [ { "ws": "", "agent_id": "", "state": "", "permission_id": null } ],
  "nodes":  [ { "ws": "", "graph_id": "", "node_id": "", "state": "", "error": null } ] }
```

### Decide banner (Depth 2)

- Placement: top of the agent dialog; a matching action row in the triage
  list; the canvas node badge (!) routes to one of those two.
- Buttons Done / Rerun / Skip (+ dismiss). `POST …/nodes/{node}/decide` with
  `{ action, reason }`. The daemon validates the node is `needs_decision`, then
  applies the engine's existing ruling path (`dag.rs:615`) — journaled, events
  published, node transitions. (The detailed design pins the exact ruling
  semantics per action against `dag.rs`.)

### Canvas upgrades

- State glyph sits beside the role glyph in the card header; `aria-label`
  carries the state name.
- `loop_back` edges: for each human-gate node with `loop_back {on?, small,
  big}`, dashed edges gate→target, labels "small"/"big", violet; animated for
  a few seconds while a `LoopBack` event is in flight.
- `on_error` tag: text chip under the card meta — "on_error: delegate | skip |
  rerun ×N".
- Idle canvas (workspace detail): same live renderer with an `idle` prop —
  last-run states at low emphasis, no spinner, caption "idle — last run
  <time>", still clickable. Not a new canvas mode.

### Backend (Phase A — scoped, minimal)

All five items are Phase A (daemon + CLI); the UI consumes them in Phase B.

1. **`GET /api/v1/triage` + `supervisor status` triage section** — read-only
   aggregate over agent states + node states; no journal. The CLI prints it;
   the UI (Phase B) renders it. Listed in the §4.16 API table in the detailed
   design. Replaces the alternative fan-out (N graphs × M workspaces × 2s).
2. **`missing_role` persistence** — persist on the `MissingRole` event (same
   `persist_node` pattern, `workflow.rs:594-618`); clear (→ `pending`/`ready`)
   on the next `NodeReady`/`NodeStarted`/role-resolution. A test covers the
   clear path explicitly — a stale `missing_role` row must never outlive its
   cause.
3. **`workspace_id` on `WorkflowEvent`** — new field, `#[serde(default)]`;
   every publish site in the workflow service attaches the workspace it knows;
   a golden test replays an old-shaped journal row (the review's wire-compat
   bar, `review_2026-08_supervisor-v2.md:242`). Closes I-26.
4. **Decide endpoint + `supervisor dag decide`** — one route, engine ruling
   path, journaled, response = new node state. The CLI command is the Phase A
   acceptance vehicle; the banner (R7) is Phase B.
5. **cmux adopt-or-create** — `on()` calls `list_workspaces()` first; if a
   cmux workspace with the deterministic name exists, adopt it (record its
   surfaces/handles) instead of `new_workspace`; `ensure_panes` still fills
   missing foreground panes. `off()` closes what the supervisor owns. This is
   the "scan cmux, read the terminals, don't duplicate" requirement (raised
   2026-08-15): the Live tab then shows true terminal state, and the test
   workspace stops duplicating on every run.

## Boundaries

**In:** R1–R12.

**Out:**

- Playwright e2e + a11y pass — the polish plan.
- Graph engine v2 (P3–P7) — separate spec; the editor must not freeze the node
  schema (`2026-08-14-supervisor-graph-engine-v2.md:524`).
- "Start new agents": no daemon capability exists to spawn new agents into a
  workspace (agents come from layout files). Recorded as deferred in the
  web-UI spec, not built here.
- Decide Depth 3 (split/split-to, decision console) — deferred.
- agent-bus bridge worker (I-34) — already deferred (`review…:215-216`).
- ratatui dashboard — untouched.

## Open questions

1. **Default dashboard tab** — Live (recommended; the dashboard represents
   what is running) or Stats?
2. **Triage placement** — strip on the Live tab (recommended) vs its own nav
   page?
3. **fg/bg filter placement** — per-workspace-card segmented control on the
   Live tab + detail page (recommended) vs one global control?
4. **Adopted cmux workspace on `off()`** — treat adopted as owned and close it
   (recommended, simple, matches I-6's recorded-pid intent) vs leave adopted
   surfaces running?

## Implementation sketch (after lock)

1. **Phase A** (order): `workspace_id` + serde compat → `missing_role`
   persist+clear → cmux adopt-or-create → decide endpoint + `dag decide` →
   triage endpoint + `status` triage section. Each with tests; gate = cargo
   green + live CLI walk.
2. **Phase B** (order): reducer + types → canvas upgrades + SSE (polls
   removed) → dashboard Live/Stats tabs (agents-first) → workspace detail
   page → agent dialog feed + decide banner → intake/rules pages + full
   property panel.
3. Ledger + spec updates (§5 polling note replaced by SSE; §13-style records
   for the functional additions; polish plan scope reduced).

## Related

- Review I-31: `docs/reviews/review_2026-08_supervisor-v2.md:206`
- Web-UI spec: `docs/specs/2026-08-14-supervisor-webui-detailed-design.md`
- Supervisor spec: `docs/specs/2026-08-13-supervisor-detailed-design.md`
  (§4.12 escalation, §4.15/4.16 API)
- Polish plan (scope-reduced sibling):
  `docs/plans/plan_high_level_2026-08_webui-polish-e2e.md`
- Product record: `PRODUCT.md` (impeccable init, 2026-08-15)


---

# Plan Feedback

I've reviewed this plan and have 1 piece of feedback:

## 1. General feedback about the plan
> aproved.. no need to open the tool anymore.. but another agent iwll build .so just prin the doc path in the termina,

---
