# agent-bus Fleet Supervisor — Web UI & Workflow Editor: Detailed Design

**Status:** Draft (build blueprint — component-by-component, implementable)
**Date:** 2026-08-14
**Depends on:** `2026-08-13-supervisor-detailed-design.md` (implemented, M1–M7),
`2026-08-13-supervisor-implementation-handoff.md`
**Audience:** coding agents, reviewers, designer

This is the level below the approved high-level plan's *Phase 2 — management
surfaces*: a loopback **web UI** that is a thin client over the already-built
supervisor API (§4.16), plus the small set of backend additions the UI's asks
(costs, agent streams, DAG editing) require. The backend contracts below were
verified against the implemented `supervisor-daemon` (`crates/supervisor-daemon/src/api.rs`)
and `supervisor-core` types on 2026-08-14.

Everything in the UI is driven by **one reusable workflow canvas** that has two
modes over the same visuals: **edit** (drag-and-drop composition) and **live**
(the running process — active nodes animate). The same component is embedded in
the dashboard, the workspace view, the DAG editor, and the agent dialog.

---

## 1. Goals

1. **Fleet dashboard** — at a glance, what every workspace/agent is doing: agent
   state, last activity, inbox depth, running workflows, and **key metrics**
   (messages delivered, errors, decisions, tokens, **cost**).
2. **Workflow editor** — create and edit DAGs with a drag-and-drop UX that
   composes a process: add nodes, wire dependencies, edit a node's properties,
   validate, save.
3. **Live process visualization** — the *same visuals* as the editor, but driven
   by the live event stream: a node whose agent is running **animates**; done /
   failed / blocked / needs-decision are color-coded; clicking a node opens the
   agent.
4. **Agent dialog** — click any node → a generic per-agent view on the web:
   the agent's message stream, live activity, and interaction (send a message,
   respond to a permission, abort, attach a pane).
5. **One reusable renderer** — the canvas is a self-contained component
   (data-in, callbacks-out, no network calls) so it can be reused across the
   supervisor UI, embedded dashboards, and future surfaces.

The existing **ratatui** dashboard stays for quick terminal use; the web UI is
the rich surface for editing and at-a-glance fleet awareness.

---

## 2. Architecture

```
supervisor-daemon (127.0.0.1:4198, bearer token)
   ├─ /api/v1/*          existing REST (workspaces, agents, graphs, rules,
   │                      decisions, proposals, intake) + NEW endpoints (§4)
   ├─ /api/v1/events     existing SSE stream of BusEvent (§4.18) — live core
   └─ /ui/*              static SPA (built bundle, served by the daemon)
        └─ SPA: Vite + React + TS
             ├─ api client (typed, fetch + Bearer)
             ├─ sse client (fetch-stream SSE; Bearer via header — not EventSource)
             ├─ store: React context + reducer (live state from events)
             ├─ pages: dashboard, workspace, graphs, editor, agent, decisions
             └─ lib/workflow-canvas: the reusable component (§6)
```

### 2.1 Stack

| Concern | Choice | Rationale |
|---|---|---|
| SPA | **Vite + React 18 + TypeScript** | ecosystem for graph UI; typed against `supervisor-core` shapes |
| Canvas | **`@xyflow/react` (React Flow v12)** | the standard DAG editor/visualizer: drag-and-drop, custom nodes, edge handles, pan/zoom — one library serves both edit and live modes |
| Auto-layout | **`dagre`** (pure) | layered layout for live/view mode and editor "auto-arrange" |
| Data fetch | **`@tanstack/react-query`** | caching/polling for REST |
| Live | **fetch-based SSE reader** (small hand-rolled hook) | `EventSource` cannot set the `Authorization` header; the API requires Bearer (§4.16). A fetch-stream SSE parser is ~60 lines |
| Styling | **CSS modules + a small design-token file** | no heavyweight framework; state colors/animations are CSS classes |
| Serving | **`tower-http` `ServeDir`** + SPA fallback in the daemon | one origin for UI + API; loopback |
| Tests | **vitest + React Testing Library** (unit/component), **Playwright** (e2e) | renderer + reducer are the testable core |
| Monorepo dir | `web/` at the workspace root | separate from the Rust crates; built by `cargo`-independent tooling (`npm`), artifacts copied to `~/.supervisor/ui` |

### 2.2 Where the SPA lives and how it is served

- Source: `web/` (package.json, Vite, `src/`). Built output → `web/dist`.
- `supervisor build-web` (or a `Makefile`/npm script) copies `web/dist` into the
  supervisor state dir `~/.supervisor/ui`.
- The daemon serves it: `GET /ui/*` → `ServeDir(~/.supervisor/ui)` with a
  fallback to `index.html` for client-side routes; `GET /` → redirect `/ui/`.
- Dev: `npm run dev` runs Vite on 5173 with `/api` proxied to 4198; the SPA
  reads the token the same way in dev and prod (§3).

### 2.3 Token bootstrap

The browser cannot read `~/.supervisor/api-token` (0600, supervisor-owned).
`supervisor web` opens the UI with the token in the URL hash:

```
open "http://127.0.0.1:<api_port>/ui/#token=<token>"
```

- The SPA reads the token from `location.hash`, keeps it **in memory only**
  (module-scope variable), attaches it as `Authorization: Bearer` on every
  request, and strips it from the URL (`history.replaceState`).
- Never stored in `localStorage`/`sessionStorage`. Loopback-only surface; the
  token is a session secret, not a credential to persist.
- A missing token shows a "run `supervisor web`" screen — never prompts, never
  guesses.

---

## 3. Backend additions (supervisor-daemon + core)

The existing API (§4.16) already covers workspaces/agents/graphs/rules/
decisions/proposals/intake + the SSE stream. The UI needs these additions. All
new endpoints stay behind the existing bearer-token auth and return JSON.

### 3.1 Agent mode surfaced (unblocks `agents --background` filter)

`Agent` records today carry no `mode`/`driver`; the layout (`supervisor.toml`)
holds them. Add `mode` and `driver` to the `agent` table (§3.1) and to the
`GET /api/v1/workspaces/{id}/agents` payload (from layout on upsert). The UI
filters foreground/background and renders background agents dimmed / headless.

### 3.2 Agent interaction + transcript endpoints (drive the agent dialog)

Widen the `AgentDriver` trait (§4.7) with two defaulted methods:

```rust
async fn read_transcript(&self, a: &AgentRef, limit: usize)
    -> anyhow::Result<Vec<TranscriptMessage>>;   // opencode: GET /session/{id}/message?limit=
                                                 // cmux: last read-screen chunk, [{role:"assistant", text}]
async fn respond_permission(&self, a: &AgentRef, permission_id: &str, allow: bool)
    -> anyhow::Result<()>;                       // opencode: POST /session/{id}/permissions/{pid}
                                                 // cmux: send the response text / Noop
```

`TranscriptMessage { role: String, ts: String, text: String, usage: Option<Usage> }`
where `Usage { prompt_tokens, completion_tokens }` is `None` for cmux.

New endpoints:

| Endpoint | Purpose |
|---|---|
| `GET /api/v1/workspaces/{ws}/agents/{aid}/messages?limit=` | transcript (driver `read_transcript`); also the usage source for the cost collector |
| `POST /api/v1/workspaces/{ws}/agents/{aid}/permissions/{pid}` | `{ "response": "allow"\|"deny", "remember": bool }` |
| `POST /api/v1/workspaces/{ws}/agents/{aid}/abort` | driver `abort` |
| `POST /api/v1/workspaces/{ws}/agents/{aid}/attach` | **upgrade existing**: actually spawn the cmux surface + `cmux send "opencode attach --session <id>"` (§4.3 attach), return the pane handle — not just the command string |

### 3.3 Usage & cost (new — nothing records this today)

- New table `usage`: `(id, workspace_id, agent_id, model, ts, prompt_tokens,
  completion_tokens)`. Tokens are stored; **cost is computed on read**.
- **Collector** (daemon service): after each `step.ended` (and on a 60s
  fallback poll), read the agent's last messages' `usage` via `read_transcript`,
  diff against the last recorded point, and insert rows. Idempotent by
  `(agent, message_ts)` — track the last seen message ts per agent.
- **Model prices** in root `supervisor.toml`:

  ```toml
  [usage]
  model_prices = { "anthropic/claude-sonnet-4" = { in_per_mtok = 3.0, out_per_mtok = 15.0 } }
  ```

  Unknown models → tokens only, cost `null` (shown as "—", never 0).
- Expose `GET /api/v1/usage?ws=&agent=&since=` → per-row usage +
  computed `cost_cents`. Costs are **estimates**, never a billing surface —
  the dashboard labels them "est."

### 3.4 Metrics aggregation (dashboard numbers)

`GET /api/v1/metrics?since=` → aggregated from the journal / event history:

```json
{
  "since": "...",
  "totals": { "messages_delivered": 0, "errors": 0, "decisions": 0,
              "nodes_done": 0, "nodes_failed": 0, "tokens": 0, "cost_cents": null },
  "per_workspace": { "iot_platform": { "...": "same shape" } },
  "per_agent": { "iot_platform/dev_01": { "...": "same shape" } },
  "time_series": [ { "ts": "...", "messages": 0, "errors": 0, "cost_cents": null } ]
}
```

Sources: inbox `delivered` events, `step.failed`/`session.error` signals,
`decision` rows, `workflow` `node.done/failed` events, and the `usage` table.
Bucket size for `time_series` = 1h (query param `bucket=1h|1d`).

### 3.5 Decision outcomes (unblocks bake-back confidence)

The gaps note "decision outcomes aren't recorded". Add an `outcome` column
write path: when the action a decision produced resolves (node done / rerun
bounded / skip), update the `decision` row. Add `POST
/api/v1/decision-log/{id}/outcome` `{ "result": "applied"|"failed", "note": "…" }`
and surface `outcome` in `GET /api/v1/decision-log`. The bake-back service then
uses it for confidence (its `min_occurrences` + success-rate logic).

### 3.6 Bundled fixes (found during review)

- **CLI `put_graph` uses POST against a PUT-only route** (`supervisor-cli/src/client.rs:159`
  vs `api.rs:75`) → 405 on every graph save. Fix to `reqwest::RequestBuilder::put`.
- **fleet.json projection (§3.3)** — the human-readable snapshot cache is not
  implemented. Add it (journal → in-memory → atomic rewrite) since the
  supervisor agent reads it.
- SSE endpoints stay bearer-authed; no query-token backdoor (the UI uses
  fetch-stream SSE with the header).

### 3.7 Static serving deps

Add to `supervisor-daemon/Cargo.toml`: `tower-http` (features `fs`), `tower`
(already transitively present via axum). No new service for the UI.

---

## 4. Frontend structure (`web/`)

```
web/
  package.json  vite.config.ts  tsconfig.json  index.html
  src/
    main.tsx
    app.tsx                 # router + providers
    api/
      client.ts             # fetch wrapper: base + bearer + error shape
      types.ts              # mirrors supervisor-core shapes (GraphDef, NodeState,
                            #   AgentState, BusEvent, Usage, …) — hand-typed, validated in tests
      endpoints.ts          # thin functions per endpoint
      sse.ts                # fetch-stream SSE parser → AsyncIterable<BusEvent>
    store/
      live-store.ts         # React context + useReducer: event → state
      reduce.ts             # PURE reducer: BusEvent[] → LiveState (§6.4) — unit-tested
    components/
      WorkflowCanvas/       # the reusable renderer (§6) — no API calls
      AgentChip.tsx         # agent state pill (used in dashboard + canvas nodes)
      Metric.tsx  TriageList.tsx  Timeline.tsx
    pages/
      Dashboard.tsx  Workspace.tsx  GraphList.tsx  GraphEditor.tsx
      Agent.tsx      Decisions.tsx  Rules.tsx
    lib/
      layout.ts             # dagre layered layout → {nodes:[{id,x,y}], edges}
      graph-edit.ts         # pure helpers: add/remove node, wire/unwire deps,
                            #   cycle guard (mirrors core validation), to/from GraphDef
```

The **only** component that may call the network is the pages layer. Everything
under `components/` and `lib/` is pure/stateless so it can be unit-tested and
reused.

---

## 5. Pages

### 5.1 Dashboard (`/`)

- **Metrics strip** (top): messages delivered, errors, decisions, nodes done,
  tokens, **est. cost** — for today, from `GET /api/v1/metrics`.
- **Triage list**: every agent in `waiting_input` / `blocked_permission`, and
  every node in `needs_decision` / `failed` / `missing_role` — clickable → agent
  dialog / editor. Driven live by SSE.
- **Workspaces**: cards per workspace — state (off/on/draining/error), agent
  roster with state chips, inbox depth, active workflows.
- **Active workflows**: for each running graph, a **mini live canvas**
  (`WorkflowCanvas mode="live"`, small size) so the whole fleet's process
  progress is visible at once.
- **Decision log** (collapsed panel): recent decisions + proposals awaiting
  approval (`apply`/`reject` inline).
- Live updates: agent state and workflow events arrive over SSE
  (`/api/v1/events`); **node state is polled** (I-26 — the current workflow
  events do not carry a `workspace_id`, so canvases read node states from
  `GET /api/v1/graphs/{id}/nodes?ws=` every ~2s, workspace-scoped since I-1.
  When workflow events gain `workspace_id`, the canvas can switch to SSE
  attribution; until then the ~2s poll is the documented mechanism).

### 5.2 Workspace view (`/workspaces/:ws`)

- Agent grid (foreground/background filter — §3.1), each with state, last
  activity, inbox depth, cost today, **click → agent dialog**.
- Workflows installed/active in this workspace as live canvases.
- Per-agent 24h cost/token mini-chart (`GET /api/v1/usage`).
- Workspace controls: `on` / `off` (graceful) / `resume`.

### 5.3 Graph list + editor (`/graphs`, `/graphs/:id`)

- List: installed graphs, active flag, version, node count.
- Editor = `WorkflowCanvas mode="edit"` full-screen:
  - Left rail: **palette** of node types (role templates: dev/reviewer/tester/
    designer/memory-keeper; blank node) — drag onto the canvas to add.
  - Canvas: React Flow nodes + edges; drag to reposition; **connect handles to
    wire `depends_on`**; select a node → **property panel** (role, agent_id,
    start_template, done_when ack/approved/match, on_error, gate, loop_back
    small/big, mode, timeout).
  - Toolbar: auto-arrange (dagre), **validate** (calls the same checks as core:
    unique ids, deps exist, no cycle, every node has a done_when criterion,
    loop_back targets exist — `lib/graph-edit.ts` mirrors them client-side),
    save (`PUT /api/v1/graphs/{id}` — §3.6 fix), cancel.
- Editing a **running** graph: allowed, but a "running" badge is shown; saving
  takes effect for the **next** run (the in-flight `Workflow` instance keeps its
  definition).

### 5.4 Agent dialog (`/workspaces/:ws/agents/:aid`)

Opened by clicking any canvas node **or** an agent chip. Generic across drivers.

- **Header**: agent id, role, model, driver, state, mode, session id.
- **Transcript**: `GET …/messages?limit=50`, rendered as chat rows (role + ts +
  text). Live: while the agent is `working`, poll every 1.5s and re-append new
  rows; also react to `step.started/ended` from SSE.
- **Activity feed**: step/tool/diff signals from SSE for this agent, as small
  timeline ticks.
- **Compose box**: send a message (`POST …/message`). Priority toggle
  (high/normal).
- **Permission banner**: when `permission.asked` is live, show the permission
  prompt with **Allow / Deny** (and "remember") → `POST …/permissions/{pid}`.
- **Actions**: Abort (current turn), Attach (spawn a cmux pane — §3.2 upgrade),
  and for background agents a "currently headless" note.
- On `error`/`needs_decision`, a banner with the context + a "decide" affordance
  (routed to the manager/human flow, out of scope here beyond surfacing).

### 5.5 Decisions / Rules / Bake-back (secondary)

Read-only lists + inline `apply`/`reject` for proposals; rules list + add (TOML
textarea); reload. These reuse existing endpoints; not part of the canvas.

---

## 6. The reusable WorkflowCanvas (the centerpiece)

### 6.1 Contract

A **pure** component: props in, callbacks out, zero network.

```ts
type NodeState = "pending" | "ready" | "running" | "done" | "failed"
               | "blocked" | "needs_decision";

interface WorkflowCanvasProps {
  graph: GraphDef;                                  // { id, name, nodes[] }
  mode: "edit" | "live";                            // live = view + animate
  nodeStates?: Record<string, NodeState>;           // live mode; absent → all "pending"
  agentStates?: Record<string, AgentState>;         // owning-agent states (overlay)
  onNodeClick?: (node: NodeDef, agentId?: string) => void;
  onChange?: (graph: GraphDef) => void;             // edit mode
  onValidate?: (issues: string[]) => void;          // edit mode
  palette?: boolean; compact?: boolean;
}
```

- **Edit mode** (`mode="edit"`): React Flow `onNodesChange`/`onConnect` drive
  `onChange`; the caller owns graph state. Node positions are editor-only
  (not persisted — the graph JSON is positions-free; layout is derived).
- **Live mode** (`mode="live"`): `nodeStates` + `agentStates` are provided;
  positions come from dagre layout (`lib/layout.ts`); interaction is
  click-to-open only. `compact` renders a smaller version for the dashboard.
- Both modes render the **same node + edge components** — this is the
  "same visuals" requirement.

### 6.2 Node rendering

Each node is a **card**: node id, role icon (role → emoji/glyph set: dev ⚙,
reviewer 🔍, tester 🧪, designer 🎨, memory-keeper 📚, manager 🧭), the owning
agent id (from role resolution / `agent_id`, or "no agent" warning for
`missing_role`), and a state treatment:

| NodeState | Visual |
|---|---|
| pending | grey, muted |
| ready | blue outline, subtle "ready" pulse |
| running | **blue fill + spinning ring / pulsing glow** — the animated "this agent is working now" state |
| done | green + ✓ |
| failed | red + ✕ |
| blocked | amber + ⛔ |
| needs_decision | amber + "!" + slow pulse |
| missing_role | grey + ⚠ (from `WorkflowEvent::MissingRole`) |

Agent-state overlay (agent chip in the card corner): `working` = spinner,
`waiting_input`/`blocked_permission` = highlight (these feed the dashboard
triage), `idle` = dim.

### 6.3 Edges

- `depends_on` → one edge per dependency. Arrowhead to the dependent.
- A node that just became `ready` (edge from a newly-`done` parent) gets an
  **animated dashed edge** for a few seconds.
- `loop_back` targets drawn as a dashed "revision" edge (human-gate nodes) in a
  distinct color, from the gate back to its `small`/`big` targets.
- `on_error` rerun/skip/delegate shown as a small tag on the node, not an edge.

### 6.4 Live store (pure reducer)

`reduce(prev: LiveState, event: BusEvent): LiveState` — unit-tested, drives
both the dashboard and every canvas:

```ts
interface LiveState {
  workspaceStates: Record<string, WorkspaceState>;
  agentStates: Record<ws, Record<agent, AgentState>>;
  nodeStates: Record<ws, Record<graph, Record<node, NodeState>>>;
  permissionPending: Record<ws, Record<agent, Permission | null>>;
  lastEvents: BusEvent[];                       // ring buffer for activity feeds
}
```

Mapping (mirrors the backend types):
- `WorkflowEvent::NodeReady/Started/Done/Failed/Blocked/NeedsDecision` →
  node state transitions (a "running" node = `NodeStarted`; revert to `ready`
  on `loop_back`).
- `FleetEvent::AgentState` → agent state.
- `Signal::PermissionAsked` → permission banner.
- `Signal::StepStarted/Ended`, `Diff` → activity feed ticks.
- `Signal::Heartbeat` → ignored for state.

The SPA subscribes once (`sse.ts`), feeds the reducer, and canvases select
their slice — no per-component connections.

### 6.5 Reuse story

Because the canvas is pure, it is dropped into: the dashboard (compact live),
the workspace view (live), the graph editor (edit), and the agent dialog
(context, live, with the clicked node highlighted). It has no dependency on the
supervisor's API or store — any caller that supplies a `GraphDef` + states gets
the same renderer. This is the "reusable in many situations" requirement.

---

## 7. Testing

**Core/frontend unit (vitest + RTL):**
- `reduce` — every event → state transition (incl. `loop_back` reverting a
  node, `missing_role`).
- `lib/graph-edit.ts` — add/remove/wire/cycle-guard (mirror of core validation).
- `WorkflowCanvas` — renders states correctly; running node has the animated
  class; edit-mode connect fires `onChange` with the new dependency; compact
  variant.
- `lib/layout.ts` — layered layout is acyclic and positions nodes.

**Backend (integration, real daemon + fake drivers):**
- New endpoints round-trip: messages, permission, abort, usage, metrics,
  decision outcome.
- Usage collector: a fake driver yields `usage` → rows written once (idempotent
  on message ts), cost computed from `model_prices`.
- `attach` now spawns a cmux surface (real cmux).

**End-to-end (Playwright against a real daemon on 4198):**
- Dashboard renders workspaces/agents/metrics and updates on SSE.
- A live mini-canvas animates when a node starts and turns green on ACK —
  **requires the workflow end-to-end chain to work live** (see §9).
- Editor: drag a node from the palette, wire a dependency, edit the property
  panel, save via PUT, re-open and see it persisted.
- Agent dialog: send a message, see it echoed in the transcript; permission
  allow/deny.

---

## 8. Milestones (U1–U6)

1. **U1 — scaffold + dashboard read**: `web/` SPA (Vite+React+TS), token
   bootstrap, typed api client + `sse.ts`, static serving + SPA fallback in the
   daemon, `fleet.json` projection, CLI `put_graph` PUT fix, agent `mode`/`driver`
   surfaced. Dashboard: workspaces, agents, metrics strip (from `metrics`
   endpoint), triage. Live via SSE.
2. **U2 — WorkflowCanvas (live mode)**: dagre layout, node/edge visuals, state
   animations, `reduce` store; embed compact canvases in dashboard + workspace
   view.
3. **U3 — agent dialog**: backend interaction/transcript endpoints + driver
   widening (§3.2); transcript + activity + compose + permission + abort +
   attach.
4. **U4 — DAG editor**: `WorkflowCanvas` edit mode — palette drag, wiring,
   property panel, client-side validation, auto-arrange, save.
5. **U5 — metrics + cost**: `usage` table + collector + `model_prices`,
   `GET /api/v1/metrics`, decision-outcome recording, dashboard cost/token
   numbers + time series, decisions/bakeback pages.
6. **U6 — polish + e2e**: `attach` spawns panes, intake/rules pages, Playwright
   suite, accessibility pass.

---

## 9. Hard dependency on the live chain

The gaps list the single biggest untested surface: **no real agent turn has run
through a node yet** (on → inbox → idle → ACK → apply). The dashboard's
mini-canvases and the agent dialog render nothing meaningful until a real node
executes. U2/U3 acceptance therefore **depends on a live smoke run** — either
closed as part of U1 (a `supervisor smoke` script that brings up a scratch
workspace, posts a start message, waits for the ACK, and reports the chain) or
the UI work is sequenced so the smoke lands first. The UI spec itself does not
change the chain; it just needs it to be observable.

---

## 10. Security

- Loopback only (`127.0.0.1`); UI and API on the same origin.
- Bearer token in memory only; stripped from the URL; never persisted.
- SSE uses the Authorization header (fetch-stream), not a query token.
- No secrets in the UI: transcripts are agent output only; `OPENCODE_SERVER_PASSWORD`
  and `api-token` never leave the daemon.
- Cost figures are estimates and labeled as such; never exposed as billing.

---

## 11. Decisions (resolved)

1. **One canvas, two modes** — React Flow renders both edit and live; the node/
   edge components are shared. This is what makes the visuals identical between
   composing and watching.
2. **SPA served by the daemon** — one origin, loopback; `supervisor web` opens
   it with the token in the URL hash. No separate auth surface.
3. **SSE via fetch-stream, not EventSource** — the API requires the Bearer
   header; EventSource cannot set headers.
4. **Positions are editor-only** — graph JSON stays positions-free; layout is
   derived (dagre) so live and edit views stay consistent and the wire format
   is unchanged.
5. **Cost = tokens × `model_prices`** — tokens are stored; `$` is computed on
   read and labelled "est." Unknown models show tokens only.
6. **`usage`/`metrics`/interaction endpoints are new daemon work** — the UI is
   otherwise a pure client over the existing API.
7. **fleet.json projection + decision outcomes + `attach`-spawns-pane + CLI
   `put_graph` fix are bundled** — they are pre-existing gaps the UI depends on
   (§3.6, §3.2, §3.5).
8. **ratatui dashboard stays** for terminal use; web is additive.

## 12. Open questions

1. Live chain smoke before or inside U1? (Recommended: a `supervisor smoke`
   script as the first U1 task — it de-risks everything downstream.)
2. Should `WorkflowCanvas` be extracted to a published package later (for the
   agent-bus dashboard or docs), or stay in-tree until phase 2's editor needs
   it elsewhere?
3. Do we want an in-UI graph **import/export** (paste a graph JSON) beyond the
   editor's save?
