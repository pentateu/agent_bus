# Fleet Supervisor — Wiring Fixes: Detailed Design

**Status:** Draft (design for the dev handoff — implement, don't re-design)
**Date:** 2026-08-14
**Depends on:** `2026-08-13-supervisor-detailed-design.md` (implemented),
`2026-08-13-supervisor-implementation-handoff.md` (addendum: the wiring audit,
which this document turns into a buildable spec)
**Audience:** dev agent

This document takes the F/M findings from the 2026-08-14 wiring audit and
specifies each fix concretely: the change, the interfaces/data shapes, the
files, and the tests. Implement in the order in §13. Do not re-design; where a
fix deviates from the original spec, the deviation is recorded inline.

---

## 1. F1 — Deliver a freshly-enqueued start message immediately

**Problem.** `InboxService` only delivers on idle signals or a `WorkspaceState(On)`
event that nothing publishes; a start message enqueued by the workflow runner
sits until an SSE idle signal a fresh session may never emit.

**Design.** Delivery becomes *enqueue-triggered*, with the idle signal as the
backpressure safety net (a second queued message is not delivered while the
agent is mid-turn — see §2).

- `supervisor-daemon/src/services/inbox.rs`, `InboxService::handle`: add an arm
  ```rust
  BusEvent::Inbox(InboxEvent::Enqueued { entry }) =>
      deliver_next(&entry.workspace_id, &entry.agent_id).await
  ```
  `deliver_next` already no-ops when the workspace is not `on`, and opencode
  queues prompts serially per session, so a busy agent simply parks the next
  message server-side — the turn-boundary contract is preserved.
- Keep the idle-signal arm: it drains a queue that accumulated while the
  workspace was off/draining.

**Tests.** `InboxService` unit: `Enqueued` → `deliver_next` called; integration:
enqueue to an `on` workspace with an idle opencode driver → message delivered
without any SSE signal.

---

## 2. F2 — Publish `WorkspaceState` from the workspace manager

**Problem.** `WorkspaceManager::on()` records state but never publishes the bus
event the inbox's drain-on-on path waits on; `off()` never publishes either.

**Design.** `WorkspaceManager` publishes the lifecycle it already owns:
- `on()`: after `fleet.upsert_workspace(&ws)` publish
  `BusEvent::Fleet(FleetEvent::WorkspaceState { workspace: ws })` (state `On`).
- `off()`: publish `Draining` (after the draining upsert) and `Off` (after the
  final upsert).
- `resume()` needs no change (it calls `on()`).

The `InboxService` `WorkspaceState(On)` drain arm (§audit F2) then fires for
real, covering messages enqueued while the workspace was off.

**Tests.** Workspace-manager integration (fake cmux + real opencode serve):
`on` publishes `WorkspaceState(On)`; an inbox entry queued before `on` is
delivered by the drain.

---

## 3. F3 — A live workflow-start path

**Problem.** The only real trigger is the GitHub poll; the `Command{start}` arm
is a stub, `POST /api/v1/ingest` never starts a workflow, and "bug-from-off"
only logs.

**Design.**

- **New API endpoint** `POST /api/v1/workspaces/{ws}/graphs/{graph}/start`:
  body `{ "vars": { "feature": "…", ... } }` (optional). Handler:
  1. If the workspace is not `on`, call `workspaces.on(ws)` (idempotent;
     brings it up — this is the "bug-from-off" driver).
  2. `workflows.start_graph(ws, graph, vars)`.
  Respond `{ "started": true, "graph": graph }`.
- **`POST /api/v1/ingest`** (existing handler): after `insert_intake`, map
  `kind` → graph (`bug` → `bug_flow`, `feature` → `feature_lifecycle`,
  else no workflow), bring the workspace `on` if off, `start_graph`, and set
  `intake.graph_id` (add `Fleet::link_intake(id, graph_id)` — journaled
  `intake.link`, or store on insert).
- **CLI** `supervisor start <ws> <graph> [--var k=v]…` → new endpoint.
- **Delete the stub**: `WorkflowRunner::handle` `Command{start}` arm is removed;
  `start` is routed by the command dispatcher (§4).

**Tests.** Integration: `POST …/start` on an `off` workspace brings it on and
starts the graph (root nodes `Ready`); `POST /api/v1/ingest {kind:"bug"}` →
workspace on + `bug_flow` started + intake row carries `graph_id`.

---

## 4. F4 — Route commands; make manager/rule rulings land

**Problem.** `Action::StartWorkflow`, `Action::Escalate`, and the manager's
`rerun|skip|done|split` rulings publish `Command{start}`, `{escalate}`, `{rule}`
— nothing consumes `escalate`/`rule`, `start` is a stub, and the ruling args
carry no `(graph, node)` context (`Situation.node` is always `None`).

**Design — one command dispatcher.** `WorkflowRunner` becomes the sole consumer
of workflow-related `HumanEvent::Command`. It gains a
`workspaces: Arc<WorkspaceManager>` (main.rs Arc-wraps the manager before
building the runner — see §13 ordering).

- **`start`** → `args = [ws, graph, vars_json?]` → ensure `on` (via
  `workspaces`) then `start_graph`.
- **`rule`** → `args = [ws, graph, node, action]` → load the instance for
  `(ws, graph)`, apply `instance.rule(node, ManagerRuling::from_str(action))`,
  handle the returned events. `ManagerRuling` gains `from_str` (`done|rerun|skip|split`).

**Node context into the situation.** `RuleService::situation()` must populate
`Situation.node`. The runner tracks `(ws, agent) → (graph, node)`; expose:
```rust
// supervisor-daemon/src/services/workflow.rs
impl WorkflowRunner {
    /// The `(graph, node)` an agent is currently working on, if any.
    pub fn running_task(&self, ws: &str, agent: &str) -> Option<(String, String)>;
}
```
`RuleService` gains `runner: Arc<WorkflowRunner>` and fills `node` from it.
The escalation then publishes `rule` with full context:
`args = [ws, graph, node, action, to?, body?]`. Keep `to/body` for `post`
rulings (already handled before publishing).

**`Action::Escalate`** no longer re-publishes `{escalate}`; it calls the
escalation path directly (`self.escalate(sit, vec![]).await`).

**Tests.** Unit: `running_task` reflects `on_ready`/`clear_running`; `rule`
command applies a manager ruling and emits `NodeDone`. Integration: rule fires
on `step.failed` → escalation → manager `rerun` → the node re-readies.

---

## 5. F5 — Supervisor workspace (`opencode serve :4199`)

**Problem.** The daemon builds `ManagerClient` but never starts the 4199 server
it talks to; the manager (C11) and supervisor agent (C13) are dead in practice.

**Design.** A daemon startup step (main.rs, after services, before API bind):
`ensure_supervisor_workspace(config, secret, shutdown)`:

1. Spawn `opencode serve --port 4199 --hostname 127.0.0.1` with
   `.current_dir(expand_home(workspace_root))` and `OPENCODE_SERVER_PASSWORD`
   (no `--dir`/`--agent`; verified). Record the PID in `~/.supervisor/supervisor.serve.pid`.
2. Wait `/global/health` (≤30s, reuse the workspace manager's poll logic —
   factor it into a shared helper `wait_for_health(client)`).
3. **Adopt-or-kill on restart**: if a recorded PID from
   `supervisor.serve.pid` is alive and answers `/global/health`, adopt; else
   kill the occupant and respawn on 4199 (same logic as a recorded workspace —
   never switch ports).
4. Kill on shutdown (drain then SIGTERM→SIGKILL), like `WorkspaceManager::shutdown`.
5. Config knob: `[supervisor] open_supervisor_workspace = true` (default true;
   `false` skips step 1–4 so tests/CI don't need opencode).

The **manager session** is created lazily by `ManagerClient::ensure_session`
(already implemented). The **supervisor agent TUI** (C13) is human-initiated
(`supervisor web`/`attach`) — not auto-spawned here (documented in §5 of the
original spec).

**Tests.** Integration (real opencode): daemon start brings up 4199 healthy;
ManagerClient escalation round-trips against it; restart adopts the live PID;
config `false` skips the server.

---

## 6. F6 — Bake-back preview/expire are actually triggered

**Problem.** Nothing calls `BakebackService::preview()` or `expire_old()`;
`supervisor bake-back --preview` is always empty.

**Design.**

- **New API** `POST /api/v1/bakeback/preview` → `bakeback.preview()`; respond
  with the newly-created proposals (plus pending, for display).
- **Daemon**: on start call `bakeback.expire_old()`; add a daily timer task
  calling `preview()` + `expire_old()` (knob `[bakeback] auto_preview = true`,
  default true; the timer is a small tokio task in main.rs).
- **CLI** `supervisor bake-back --preview` first POSTs `/bakeback/preview`,
  then lists proposals (existing listing stays for `--apply`/`--reject`).

**Tests.** Integration: seed decisions → `POST /bakeback/preview` creates a
`proposal_<ulid>` with `cluster_size` ≥ min; `expire_old` marks stale pending
proposals `expired`.

---

## 7. M1 — `Situation.last_output` is populated

`RuleService::situation()` becomes async and fills `last_output` from the
driver:
```rust
if let Ok((driver, agent_ref)) = self.drivers.for_agent(ws, agent).await {
    last_output = driver.read_last_output(&agent_ref, 20).await.ok();
}
```
`RuleService` gains `drivers: Arc<DriverRegistry>` (already in scope in
main.rs). Unknown/failed reads degrade to `None` (rules that key on
`last_output` simply don't match).

**Tests.** Unit: a fake driver returning output → situation carries it.

---

## 8. M2 — `StepEnded` is informational, not a turn boundary

**Problem.** `core/state.rs` maps `StepEnded` → Idle; a multi-step turn flickers
Working→Idle→Working. (ACK delivery already keys only on `SessionIdle`, so this
is a state/UX correctness fix.)

**Design.** `machine_action`: `Signal::StepEnded` → `None` (like `Diff`).
`StepStarted` still → Working. **Deviation record:** §8 of the original spec
listed `session.next.step.ended → idle`; that was wrong for multi-step turns —
idle is only `session.idle` / `status: idle`. Update the comment and the
`transition` tests.

**Tests.** `transition(Working, StepEnded)` == `None`; `transition(Working,
SessionIdle)` still → Idle.

---

## 9. M3 — Workflows survive a daemon restart

**Problem.** `WorkflowRunner`'s `instances`/`running`/`deadlines`/`vars` are
in-memory; a restart strands a mid-flight node (`Running` in the DB, no runner
to advance/ACK/timeout it).

**Design.**

- **Persist starts.** New journal type `workflow.start`:
  - `supervisor-core/src/journal.rs`: `JournalType::WorkflowStart`,
    `as_str = "workflow.start"`, `parse` arm; payload
    `{ "ws": "…", "graph": "…", "vars": {…} }`.
  - `Fleet::record_workflow_start(ws, graph, vars)` (journal-first, keeps an
    in-memory `Vec<(String,String,Map)>`); replay applies it.
  - `WorkflowRunner::start_graph` calls it after inserting the instance.
- **Restore.** `WorkflowRunner::restore()` (called at the top of `run()`):
  1. For each `(ws, graph)` recorded as started, rebuild the instance from
     `fleet.graph(graph)`, set node states from `fleet.node_states(graph)`
     with **`Running → Ready`** (at-least-once: the start message is
     re-delivered; the agent's task-id idempotency absorbs the duplicate),
     keep `Done/Failed/Blocked/NeedsDecision`.
  2. Restore `vars` from the recorded start.
  3. Publish `NodeReady` for every node now `Ready` so start messages
     re-enqueue and deliver (§1 makes that fire).
  - The `running`/`deadlines` maps are deliberately not restored — a restarted
    daemon cannot know which turn belongs to which node; `Running → Ready` is
    the safe, honest choice.
- **Start dedupe**: `start_graph` must not start twice for the same
  `(ws, graph)` while an instance exists (guard on the `instances` map).

**Tests.** Unit: restore maps `Running → Ready` and re-publishes readiness.
Integration: start a graph, kill the daemon mid-node, restart → the node
re-delivers and completes.

---

## 10. M4 — Agent `mode` persisted and surfaced

- `supervisor-core/src/types.rs`: `Agent` gains
  `#[serde(default)] pub mode: AgentMode`.
- `supervisor-daemon/src/db.rs`: `agent` table gains
  `mode TEXT NOT NULL DEFAULT 'foreground'`; `upsert_agent` writes it;
  `AgentMode::from_db`/`to_db` added (mirror `DriverKind`).
- `WorkspaceManager::ensure_sessions` sets `mode: roster.mode`.
- The API already serializes the whole `Agent` — `mode` flows through.
- CLI `supervisor agents --background`: replace the empty `if` with a real
  filter (`agent["mode"] == "background"`), and print `mode`.

**Tests.** Round-trip: a background roster agent persists `mode` through
reopen; API lists it; `--background` filters correctly.

---

## 11. M5–M10 — smaller fixes

**M5 — CLI `put_graph` uses PUT.** `supervisor-cli/src/client.rs`: add a `put`
helper (`reqwest::blocking` `RequestBuilder::put`) and use it in `put_graph`.
(Avoids the 405 on every graph save; the web UI must also use PUT.)

**M6 — `supervisor log` output.** `supervisor-cli/src/main.rs` `log()`: drop the
bogus `decision.as_f64()` column; print `ts`, `signature`, and a compact
`decision` summary (the action value inside the JSON, e.g.
`decision.action`), plus `outcome` when present.

**M7 — SSE resolver without `try_lock`.** `WorkspaceManager` keeps a
`session_index: Mutex<HashMap<SessionId, (String, String)>>` populated in
`ensure_sessions` (session → (ws, agent)). The observer resolver closure reads
this std-mutex map instead of `fleet.try_lock()` — cheap, never drops a signal
under async contention. Entries may outlive an `off`; harmless.

**M8 — `attach` spawns the pane.** `WorkspaceManager::attach_agent(ws, agent)`:
require the workspace `on` + a session; `cmux.new_surface(cmux_ws, project)` +
`cmux.send_cmd(surface, "opencode attach http://127.0.0.1:<port> --session <id>")`;
record the surface in the pane map (§M9); return the handle. API `attach`
calls it; if cmux is unavailable, fall back to returning the attach command
string with a `"spawned": false` note.

**M9 — `FocusPane` acts.** `WorkspaceManager` records
`panes: Mutex<HashMap<(ws, agent), CmuxHandle>>` in `ensure_panes` and
`attach_agent`; add `focus_agent(ws, agent)` → `cmux.focus_pane`. `RuleService`
gains `workspaces: Arc<WorkspaceManager>`; `Action::FocusPane` calls it
(remove the log-only stub).

**M10 — bundled known gaps.**
- **Decision outcome recording**: add `POST /api/v1/decision-log/{id}/outcome`
  `{ "result": "applied"|"failed", "note": "…" }` → sets `decision.outcome`.
  `RuleService::act` records `outcome: {"status":"acted"}` on execution so
  bake-back sees a success signal immediately; callers may overwrite with a
  real result later. Bake-back's confidence keeps using recorded outcomes.
- **`fleet.json` projection (§3.3)**: a small writer — after each journal-first
  mutation, atomically rewrite `~/.supervisor/fleet.json` from the in-memory
  fleet (tmp + rename). Implement as `Fleet::write_projection()` called from
  the daemon's post-mutation path (or a low-frequency snapshot task, 5s).

---

## 12. main.rs wiring order (after M5/M9/F4 changes)

The daemon's `run()` builds dependencies in this order so Arcs are available
where needed:

```
config, fleet, secret, token
ensure_default_graphs, discover_projects
shared_bus
manager (ManagerClient)                     // needs nothing at build
drivers (DriverRegistry)
let workspaces = Arc::new(WorkspaceManager::new(...))   // Arc BEFORE services that take it
workflows (WorkflowRunner::new(…, workspaces.clone()))  // F4 start routing
inbox (InboxService)                          // F1/F2
tracker, rules (RuleService::new(…, drivers, runner, workspaces))  // M1, F4, M9
bakeback (BakebackService)
ingest (IngestionService)
ensure_supervisor_workspace(...)              // F5 (after services, before API)
bakeback.expire_old() + start auto_preview timer  // F6
spawn service tasks; resume; bind API
```

---

## 13. Implementation order

1. **F1 + F2** (delivery is the spine of everything).
2. **M4** (mode) and **M5/M6** (CLI correctness) — small, independent.
3. **F3** (start endpoint + ingest wiring + CLI `start`).
4. **F4** (command dispatcher + `Situation.node` + rulings).
5. **M7** (SSE resolver) and **M8/M9** (panes/focus) — touch the workspace
   manager together.
6. **F5** (supervisor workspace) — unblocks the manager.
7. **M1** (last_output) — needs `drivers` in rules.
8. **F6** (bake-back preview/expire).
9. **M3** (workflow restart restore) — last; depends on F1/F4 being solid.
10. **M2** (StepEnded) — trivial, anytime.
11. **M10** (decision outcome + fleet.json projection).

Then extend the **web UI design** (`2026-08-14-supervisor-webui-detailed-design.md`)
acceptance with a `supervisor smoke` that proves on→inbox→idle→ACK→apply live
(needs F1–F5).

> **Deviation recorded (F-6, 2026-08-15):** `supervisor smoke` does NOT build
> its own scratch workspace + background agents; it operates on a
> caller-supplied workspace (run `supervisor add` / `on` first). It asserts
> hops 1–4 (on, start, deliver→Running, ACK→Done) and fails on a re-run
> (`already_running`); hop 5 (next node Ready) is reported, not asserted.
> The scratch-fixture harness is deferred with the Graph Engine v2 cycle.

## 14. Test expectations

- Every fix has a unit test in its crate and an integration test against a real
  daemon (+ fake cmux where applicable). Keep `cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt` green.
- The single most important check: a **live end-to-end run** of one node
  (enqueue → deliver → busy → idle → ACK → node done → next ready) — the audit's
  biggest untested surface. Sequence it after F1–F5.
