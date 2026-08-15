# Handoff: implement the Fleet Supervisor (agent-bus Orchestration v2)

**From:** designer / reviewer
**To:** dev agent
**Date:** 2026-08-13
**Status:** Ready to build

## Read first

1. `docs/specs/2026-08-13-supervisor-detailed-design.md` — **the build
   blueprint.** Implement from this document; it is the source of truth and has
   been through two review rounds (15 + 9 findings, all fixed).
2. `docs/specs/2026-08-06-agent-bus-design.md` — the existing bus, which we
   **keep unchanged** and use only as a cross-harness bridge (§7.3).
3. `docs/specs/2026-08-10-orchestration.md` — superseded by the new spec; do
   not implement from it.

## Wiring audit (2026-08-14) — gaps found in review, close before/with M5+UI

A systematic review of the implemented chain surfaced a class of **wiring
gaps**: events published with no consumer, triggers that never fire, and
command stubs. The two fundamental ones were already known (no delivery on
enqueue; no live way to start a workflow); the audit found the rest of the
class. Each is code-verified; fix the **F-series** before relying on the live
chain, treat the **M-series** as required polish.

### F — fundamental (chain blockers)

- **F1. Nothing delivers a freshly-enqueued start message.** `InboxService`
  reacts to `WorkspaceState(On)` and idle signals only — **not to
  `InboxEvent::Enqueued`** (`supervisor-daemon/src/services/inbox.rs:112-133`).
  A start message enqueued by the workflow runner sits undelivered until an SSE
  idle signal, which a fresh, never-prompted session may never emit. **Fix:**
  handle `InboxEvent::Enqueued` → `deliver_next(ws, agent)`.
- **F2. `WorkspaceState(On)` is never published.** `WorkspaceManager::on()`
  writes fleet state but never emits the bus event the inbox's drain-on-on path
  waits for (`supervisor-daemon/src/services/workspace.rs:163-177`). **Fix:**
  publish `FleetEvent::WorkspaceState{On}` after the upsert.
- **F3. No live way to start a workflow.** `WorkflowRunner`'s
  `HumanEvent::Command{start}` arm is a no-op stub
  (`supervisor-daemon/src/services/workflow.rs:129-133`); `POST /api/v1/ingest`
  only inserts an intake row, never `start_graph`
  (`supervisor-daemon/src/api.rs:472-507`); `start_workflow_for_kind` logs
  "workspace must be on" but doesn't bring it up
  (`supervisor-daemon/src/services/ingest.rs:182-186`). **Fix:** wire the
  ingest handler → `on(ws)` if off → `start_graph(ws, kind→graph)`, and add
  `POST /api/v1/workspaces/{ws}/graphs/{graph}/start` (+ `supervisor start`),
  deleting the stub.
- **F4. Rule/manager rulings go nowhere.** `Action::StartWorkflow`,
  `Action::Escalate`, and the manager's `rerun|skip|done|split` path publish
  `HumanEvent::Command{start}`, `{escalate}`, `{rule}` — and **no service
  consumes any of them** (`supervisor-daemon/src/services/rules.rs:208-271`).
  **Fix:** the workflow runner listens for `start`/`rule`; the rule service (or
  a small command dispatcher) listens for `escalate`.
- **F5. The supervisor workspace (`opencode serve :4199`) is never started.**
  `main.rs` builds the `ManagerClient` but no server on 4199 ever runs, so
  every manager escalation (C11) and the supervisor agent (C13) are dead in
  practice (`supervisor-daemon/src/main.rs:72-73`). **Fix:** §5 step 4 (ensure
  supervisor workspace + serve on 4199) is not implemented — implement it.
- **F6. Bake-back proposals are never generated.** Nothing calls
  `BakebackService::preview()` or `expire_old()`; the API only
  lists/applies/rejects, so `supervisor bake-back --preview` is always empty
  (`supervisor-daemon/src/services/bakeback.rs:40`). **Fix:** add a
  generate/preview endpoint (or run preview on a timer) + expire on start.

### M — minor but real (fix as part of the relevant milestone)

- **M1. `Situation.last_output` is hardcoded `None`** — rules can't use
  `{last_output}` or output context (`supervisor-daemon/src/services/rules.rs:152`).
- **M2. `StepEnded` maps to Idle** in the state machine — a step boundary isn't
  a turn boundary; mid-turn Working→Idle flicker in the dashboard
  (`supervisor-core/src/state.rs:54-56`). ACK delivery is unaffected (it only
  listens to `SessionIdle`).
- **M3. Workflow instances are in-memory only** — `instances`/`running`/
  `deadlines`/`vars` aren't rebuilt on restart, so a mid-flight node can't be
  advanced/acked/timed-out after a daemon restart
  (`supervisor-daemon/src/services/workflow.rs:40-47`).
- **M4. Agent `mode` isn't persisted or surfaced** — the `Agent` record has
  `driver` but no `mode`; `supervisor agents --background` filter is a no-op
  (empty body) (`supervisor-cli/src/main.rs:447-466`;
  `supervisor-daemon/src/services/workspace.rs:360-371`).
- **M5. CLI `dag apply` / graph save is broken** — `put_graph` POSTs to a
  PUT-only route → 405 on every save (`supervisor-cli/src/client.rs:158-160` vs
  `supervisor-daemon/src/api.rs:75`). **Fix: use PUT.**
- **M6. `supervisor log` prints `decision` as both string and float** — the
  second column is nonsense (`supervisor-cli/src/main.rs:241-246`).
- **M7. SSE session resolver uses `fleet.try_lock()`** — under contention it
  drops the signal (an idle signal missed = a delivery missed)
  (`supervisor-daemon/src/services/workspace.rs:413-422`). Needs an async
  resolver or a lock-lease.
- **M8. `attach` returns the attach string, doesn't spawn the pane** — known
  gap (`supervisor-daemon/src/api.rs:249-274`).
- **M9. `Action::FocusPane` only logs** — cmux focus not wired
  (`supervisor-daemon/src/services/rules.rs:218-220`).
- **M10. Known from before:** `fleet.json` projection (§3.3) not implemented;
  decision `outcome` never recorded (bake-back confidence sees no outcomes);
  bug-from-off intake doesn't drive workspace-on; `rules reload` needs a
  `rules.toml` to exist; SSE observer never observed firing (needs a live turn).

**Suggested ordering:** F1+F2 (delivery) → F3 (start trigger) → F5 (supervisor
workspace, unblocks manager) → F4 (rulings) → F6 (bake-back) → a `supervisor
smoke` script that proves the on→inbox→idle→ACK→apply chain live → then the
web UI phase (`2026-08-14-supervisor-webui-detailed-design.md`), which is
otherwise blocked on an observable live chain.

---

## What we are building

One long-lived **supervisor** process (Rust, tokio) that owns every managed
project's agents. Per project: one `opencode serve` on a fixed loopback port +
a cmux workspace of panes (one pane per foreground agent, attaching to the
shared server). The supervisor runs offline rules → LLM fallback → bake-back
learning, drives workflows (DAGs) over a layered ACK contract, and exposes a
CLI + loopback HTTP API + ratatui dashboard.

## Non-negotiables (learned in review — do not reintroduce)

- `opencode serve` has **no `--dir` / `--agent`** flags (verified). Spawn with
  `.current_dir(project)`; set agent/role/model per-session via `POST /session`.
- `cmux new-surface` has **no `--command`** (verified). Create a terminal
  surface with `--working-directory`, then `cmux send "opencode attach ..."`.
  Native alternative: `--type agent-session --provider opencode`.
- Structured output (`format: json_schema` on `prompt_async`) is **accepted but
  model-dependent** — thinking-mode models reject it. The ACK resolver MUST be
  layered (structured → parse JSON → regex → match → timeout), never structured-only.
- The **journal is the source of truth**; SQLite + `fleet.json` are rebuildable
  projections. Never two masters.
- Ports 4198 (API) and 4199 (supervisor workspace) are **reserved** — never
  allocated to a project.
- Resume uses **adopt-or-kill** on recorded ports (PID match + `/global/health`
  check; never switch ports for a recorded workspace).
- The manager (C11, background decision engine) and supervisor agent (C13,
  foreground human TUI) are **distinct sessions** — do not conflate them.

## Crate layout

```
crates/
  supervisor-core/     # PURE: types, port math, state machine, rules, DAG, ACK resolver, journal model. No I/O, no async. #![forbid(unsafe_code)], clippy::pedantic clean.
  supervisor-daemon/   # the long-lived process: all async services + clients (opencode, cmux, SSE, manager).
  supervisor-cli/      # `supervisor` command: daemon/status/on/off/resume/log/rules/bake-back/dag/api/dashboard/add/attach/agents/ingest.
```

Follow the module map in §2.3. Match the existing agent-bus workspace style
(edition 2024, `thiserror` in core, `anyhow` at binary boundary, tokio only in
the daemon, serde on the wire, no `unwrap()` outside tests).

## Suggested milestones (in order)

1. **M1 — core**: types, port allocator (with reserved set), state machine,
   rule engine (+ counters), DAG engine (+ role→agent resolution, human-gate
   `loop_back`), ACK resolver, journal model. 100% unit-tested.
2. **M2 — store + bus**: SQLite schema (§3.1), journal replay, internal event
   bus.
3. **M3 — clients**: opencode client (driver) + cmux client + the
   `AgentDriver` trait.
4. **M4 — workspace manager**: `on`/`off`(graceful)/`resume` with foreground +
   background agents, adopt-or-kill, panels closed on `off`.
5. **M5 — delivery + workflows**: per-agent inboxes, prompt delivery, the
   default `feature_lifecycle` graph (§4.11) + bug flow, ACK contract end-to-end.
6. **M6 — decision layer**: rule wiring, manager escalation with layered
   fallback, decision log, bake-back (proposal lifecycle).
7. **M7 — CLI + API + dashboard**: full command surface (§4.15), axum API
   (§4.16), ratatui dashboard.
8. **M8 — ingestion + launchd + supervisor agent**: github/app-feedback/CLI
   adapters (§4.17), launchd plist (§5), slash commands (§4.14).
9. **M9 — cmux driver (future, only if time)**: drive Claude Code / Pi / Codex
   via panes; driver trait only, no core changes.

## Key external contracts (all verified live)

- **opencode**: `POST /session/{id}/prompt_async` → 204 (serially queued);
  `GET /session/status` → map, **idle sessions omitted**; idle arrives only on
  SSE `/event`; `POST /session/{id}/permissions/{pid}` for auto-respond;
  `abort`/`revert`/`summarize`; `GET /session/{id}/message?limit=`. §7.1.
- **cmux**: `ping`, `new-workspace --name --cwd`, `new-surface --type terminal
  --working-directory` (+ `send`), `focus-pane`, `select-workspace`,
  `read-screen --lines`, `send`, `close-surface`, `close-workspace`, `notify`,
  `events --reconnect --cursor-file`. §7.2.
- **agent-bus**: unchanged; bridge worker = outbound `post` + inbound `wait`
  loop per partition (§7.3).

## Test expectations

§11 defines the strategy. Minimum bar per milestone: core is unit-tested;
daemon is integration-tested against real SQLite + a fake cmux + a real
`opencode serve`; the cmux client is tested against the real cmux app.

## Verification

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # pedantic clean
cargo fmt --all -- --check
```

If anything in the spec is ambiguous or contradicts the live tools, stop and
ask rather than guessing — the whole point of the two review rounds was to
eliminate gaps.
