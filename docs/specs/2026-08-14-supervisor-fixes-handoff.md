# Handoff — Fleet Supervisor: Wiring Fixes → Web UI

**From:** designer / reviewer
**To:** dev agent
**Date:** 2026-08-14
**Status:** Ready to build

## Read first (in this order)

1. **`docs/specs/2026-08-14-supervisor-wiring-fixes-design.md`** — **the build
   blueprint for this hand.** Every F/M fix is specified concretely there:
   the change, the interfaces, the files, the tests. Implement from it.
2. **`docs/specs/2026-08-13-supervisor-detailed-design.md`** — the original
   detailed design (already implemented for M1–M7). Use it for context on any
   component you touch; it is the source of truth unless the wiring-fixes doc
   records a deviation (e.g. M2 `StepEnded`).
3. **`docs/specs/2026-08-14-supervisor-webui-detailed-design.md`** — the phase
   that comes **after** the fixes land. Do not start it until the F-series is
   done and the live chain is proven (§4).
4. **`docs/specs/2026-08-13-supervisor-implementation-handoff.md`** — the
   original handoff + the wiring-audit addendum (the findings this hand fixes).

The codebase is `crates/supervisor-core`, `crates/supervisor-daemon`,
`crates/supervisor-cli` (workspace members). The implementation is well along:
core (rules/ack/dag/ports/bakeback/signal/config/journal/state/graphs), daemon
(opencode+cmux+SSE+manager clients, workspace/inbox/workflow/agent-state/
rules/bakeback/ingest services, SQLite+journal store, axum API, SSE `/events`),
and CLI (daemon/status/on/off/resume/log/rules/bake-back/dag/add/attach/agents/
ingest/install). **M1–M7 are built; this hand closes the audit gaps, then the UI.**

## What this hand delivers

**Phase A — the wiring fixes (F1–F6 fundamental, M1–M10 minor).** These make the
bus actually drive a workflow live and unblock the manager/bake-back. This is
the immediate work. Details + code-level design: `wiring-fixes-design.md` §1–§12.

**Phase B — the web UI** (dashboard + DAG editor + live canvas + agent dialog).
Only after Phase A proves the chain live. Blueprint: `webui-detailed-design.md`.

## The fixes at a glance (full spec in wiring-fixes-design.md)

### F — fundamental (do these first, in this order)

| # | Fix | Key change |
|---|-----|-----------|
| F1 | Deliver on enqueue | `InboxService::handle` reacts to `InboxEvent::Enqueued` → `deliver_next` (idle signal becomes the backpressure net, not the only trigger) |
| F2 | Publish `WorkspaceState` | `WorkspaceManager::on()` publishes `FleetEvent::WorkspaceState{On}` (and `Draining`/`Off` on teardown) so the inbox drain-on-on fires |
| F3 | Live workflow start | New `POST /api/v1/workspaces/{ws}/graphs/{graph}/start` (+ `supervisor start <ws> <graph>`); `POST /api/v1/ingest` brings the workspace on and starts the kind→graph workflow; delete the `Command{start}` stub |
| F4 | Route commands | `WorkflowRunner` is the command dispatcher (`start`, `rule`); `Situation.node` populated from a new `running_task(ws, agent)`; manager rulings reach the DAG; `Action::Escalate` calls the escalation path directly |
| F5 | Supervisor workspace | Daemon startup spawns `opencode serve --port 4199` (`.current_dir(workspace_root)`, adopt-or-kill on restart, `[supervisor] open_supervisor_workspace` knob); unblocks the manager (C11) |
| F6 | Bake-back triggered | `POST /api/v1/bakeback/preview` + daemon start `expire_old()` + daily auto-preview timer; CLI `--preview` generates then lists |

### M — minor (spec in §7–§11)

| # | Fix |
|---|-----|
| M1 | `Situation.last_output` populated from the driver |
| M2 | `StepEnded` no longer maps to Idle (deviation from §8; idle is `session.idle`/`status:idle` only) |
| M3 | Workflows survive restart: journal `workflow.start`, `restore()` rebuilds instances, `Running → Ready` |
| M4 | Agent `mode` persisted (DB column + `Agent` field + `ensure_sessions`), surfaced via API; `agents --background` filters for real |
| M5 | CLI `put_graph` uses PUT (fixes the 405 on every graph save) |
| M6 | `supervisor log` output corrected (drop bogus float column) |
| M7 | SSE resolver reads a cached session map instead of `fleet.try_lock()` (no dropped signals) |
| M8 | `attach` actually spawns the cmux pane |
| M9 | `Action::FocusPane` focuses via cmux |
| M10 | Decision `outcome` recording endpoint + auto-"acted" outcome; `fleet.json` projection writer |

## Non-negotiables (learned in review — do not reintroduce)

- `opencode serve` has **no `--dir`/`--agent`** — spawn with `.current_dir(project)`; agent/role/model per-session via `POST /session`.
- `cmux new-surface` has **no `--command`** — create `--type terminal --working-directory`, then `cmux send "opencode attach …"`.
- Structured output (`format: json_schema`) is **model-dependent** — the ACK/manager resolvers stay layered, never structured-only.
- **Journal is the source of truth**; SQLite + `fleet.json` are projections. Journal-first, always.
- Ports 4198/4199 are **reserved** — never allocated to a project.
- Resume = **adopt-or-kill** on recorded ports (PID + `/global/health`; never switch ports).
- Manager (C11) and supervisor agent (C13) are **distinct sessions**.
- `#![forbid(unsafe_code)]`, `clippy::pedantic` clean, `thiserror` in core, `anyhow` at binary boundaries, no `unwrap()` outside tests.

## Implementation order (from wiring-fixes-design.md §13)

1. F1 + F2 (delivery is the spine)
2. M4, M5, M6 (independent, small)
3. F3 (start endpoint + ingest + CLI `start`)
4. F4 (command dispatcher + node context + rulings)
5. M7 + M8 + M9 (touch the workspace manager together)
6. F5 (supervisor workspace — unblocks the manager)
7. M1 (needs `drivers` in rules)
8. F6 (bake-back)
9. M3 (workflow restart — depends on F1/F4)
10. M2 (trivial)
11. M10 (decision outcome + fleet.json)

**main.rs dependency order** after the changes (Arc-wraps matter):
`config/fleet/secret/token → graphs/discovery → bus → manager → drivers →
Arc<WorkspaceManager> → workflows(runner takes workspaces) → inbox → tracker →
rules(takes drivers, runner, workspaces) → bakeback → ingest →
ensure_supervisor_workspace → expire_old + auto-preview timer → spawn tasks →
resume → bind API`.

## Verification — the bar for "done"

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Plus the one thing that has never run live and is **the** acceptance test for
Phase A: a **live end-to-end run of one node** —

```
workspace on → start graph → root node Ready → start msg enqueued
→ delivered to the agent (driver.send) → busy → idle (SSE)
→ layered ACK resolved → node Done → next node Ready
```

Implement a `supervisor smoke` command (or a test harness script) that drives
this against a real `opencode serve` + a scratch workspace with background
agents, and asserts each hop via `GET /api/v1/graphs/{id}/nodes`. Until this is
green, Phase B (web UI) does not start — its live canvases/agent dialog render
nothing without an observable live chain.

## After Phase A

Open **`docs/specs/2026-08-14-supervisor-webui-detailed-design.md`** and build
per its milestones U1–U6 (scaffold+dashboard → WorkflowCanvas live → agent
dialog → DAG editor → metrics+cost → polish+e2e). Its backend additions
(usage/cost collector, transcript/permission/abort endpoints, agent `mode`
surfacing, static SPA serving + token bootstrap) are specified there; note that
**M4 (mode)** and **M5 (graph save)** from this hand are prerequisites it
depends on, so they are in Phase A deliberately.

## If anything is ambiguous

Stop and ask rather than guessing. Every design doc has been through review;
the whole point is that the dev implements without re-designing. If a live-tool
contract contradicts a doc (as `serve --dir` and `new-surface --command` did),
flag it — do not silently "fix" the design.
