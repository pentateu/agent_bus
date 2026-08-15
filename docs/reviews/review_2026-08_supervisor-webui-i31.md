# Review: supervisor web UI — I-31 Phase A (daemon + CLI)

**Date:** 2026-08-15
**Mode:** diff-scoped review of `854819d..82bc5bb` on `feature/supervisor-webui-i31`
(8 commits, 10 files, +730/−14). Review performed in the worktree
`.worktrees/feature/supervisor-webui-i31` (tests, builds, code reads) — the
main checkout was only used for this report.
**Verdict:** APPROVE WITH CHANGES

## Summary

Phase A implements the plan faithfully: A1's bus-boundary `workspace_id`, A2's
surface-only `missing_role` marker with recheck, A3's cmux adopt-or-create
(with a live-caught `list-workspaces` shape fix), A4's journal-first decide
endpoint + CLI, and A5's triage endpoint + `status` section all match
`docs/plans/plan_2026-08_supervisor-webui-i31.md` §3–§6. Two Important gaps
remain: the plan's "workspace on" recheck trigger is not implemented, and the
CLI `dag decide` resolves the workspace by first-match, which is ambiguous in
the multi-workspace case that I-1 made real. Fix those two, then Phase B.

## Verification performed

- `cargo test --workspace` (worktree) → **423 passed** (matches claim)
- `cargo clippy --workspace --all-targets -- -D warnings` (worktree) → clean
- `cargo fmt --all -- --check` (worktree) → clean
- `npm ci && npm run test` (worktree) → **24 passed**; `npm run build` → clean
- Full diff read; publish sites, triggers, and off() paths cross-checked in
  the worktree

## Findings

### Important

**P-1 — The plan's "workspace on" recheck trigger for A2 is not implemented.**
Plan §6 (A2): "Triggers: `FleetEvent::AgentState` where the new state is Idle
or Working (a session exists), **and workspace `on`**." Only the AgentState
trigger exists (`services/workflow.rs:354`); `ensure_sessions`/`on()` publish
only `FleetEvent::WorkspaceState` (`services/workspace.rs:292,325,354`), never
an AgentState event, and nothing calls `recheck_missing` from the on path.
Concrete failure: a workspace resumes with agents whose fleet state is already
`Idle` (resume reuses sessions; `Idle → Idle` is not a transition, so no event
fires) → a node held on `MissingRole` from before the off stays held
indefinitely — even though the workspace is on and the roster is staffed —
until some unrelated agent-state event happens to fire. Fix: call
`recheck_missing(ws)` at the end of `on()` (after `ensure_sessions`), or
publish AgentState events from `ensure_sessions`.

**P-2 — `supervisor dag decide <graph> <node>` is ambiguous across workspaces.**
`supervisor-cli/src/main.rs:643-651` resolves the workspace by first-match over
`client.graph_nodes(None, graph)` — every node row for that graph across all
workspaces — with the comment "a graph runs in at most one workspace". I-1
established exactly the opposite (that is why node rows are workspace-keyed):
two workspaces can run the same graph. In that case the CLI silently rules on
whichever workspace sorts first — a human decision applied to the wrong
workspace's node. Fix: add `--ws <ws>` to `dag decide` and require it when the
graph's node rows span more than one workspace (error: "N workspaces run this
graph — pass --ws").

### Minor

- **P-3 — decide endpoint classifies errors by substring match.**
  `api.rs` decide handler: `msg.contains("not needs_decision")` → 409,
  `msg.contains("unknown")` → 404, else 400. Correct today, but any wording
  change in the runner's errors silently flips status codes. Prefer a typed
  error from `runner.decide`.
- **P-4 — The A3 off() test does not exercise off().**
  `adopted_cmux_workspace_is_closed_on_off` calls `cmux.close_workspace`
  directly; the real `off()` path (workspace.rs:332-333 — present in code,
  closes any recorded `cmux_ws`, satisfying locked decision 4) is untested.
  Plan §8 requires "off() closes adopted".
- **P-5 — decide() proceeds even when the ruling journal append fails.**
  The `append_decision` error is logged and the transition still runs —
  consistent with the daemon's existing start_graph pattern, but a restart can
  lose the DecisionRecord (the node transition itself is journaled separately
  via `persist_node`, so only the ruling history is at risk).
- **P-6 — The MissingRole row is double-persisted.**
  `on_ready` persists the marker directly and also publishes
  `BusEvent::Workflow{MissingRole}`, which loops back through the runner's own
  subscriber into `handle_event`'s MissingRole arm, persisting again.
  Idempotent (same value), but adds a duplicate journal line per hold.
- **P-7 — `~` expansion with `HOME` unset yields `/<rest>` (root-relative).**
  `api.rs` register path: `HOME.unwrap_or_default() + "/" + rest`. Should
  error instead of pointing at `/`.
- **P-8 — CLI `--reason` is optional; the plan shows it required (§5.2).**
  Benign (the record stores the empty default), but record the deviation or
  enforce it.

## What is correct

- **A1** — `BusEvent::Workflow { workspace_id, event }` at the bus boundary
  exactly per plan §3.2; both publish sites (`workflow.rs:463,582`) attach the
  workspace; the engine's `WorkflowEvent` and the journal's
  `WorkflowTransitionEvent` are untouched (wire-compat preserved); roundtrip
  tests assert the new shape; the old web build no-ops safely (verified: web
  tests pass unchanged against the new daemon shape).
- **A2** — `NodeState::MissingRole` is a true surface marker (engine never
  sets it; holds at `Ready`); db codec round-trips both directions with
  unknown-string → `Pending` fallback; clear-on-transition is tested (an
  appearing agent flips the row to `Running` via the recheck path).
- **A3** — adopt-or-create matches by the deterministic workspace name; the
  `list-workspaces --json` object shape (`workspaces[]` + `custom_title`) is
  parsed correctly (the live-caught bug is fixed and tested); adoption records
  `cmux_ws` through the existing upsert so it survives restart and off()
  closes it; missing foreground panes still get created.
- **A4** — journal-first: the `DecisionRecord` (signature
  `human.ruling.<graph>/<node>`, plan-shaped situation/decision, source human)
  is written before the engine ruling is applied; done/rerun/skip all
  transition correctly; double-decide is a 409; unknown graph/node → 404;
  tests cover rerun/done/skip/unknown/bad-action.
- **A5** — triage returns exactly the plan's filters (agents:
  waiting_input/blocked_permission/error; nodes:
  needs_decision/failed/blocked/missing_role); CLI `status` renders the
  triage section including the "nothing needs attention" case.
- **Live-gate fixes** — the cmux list-workspaces shape fix and the `~`
  expansion in discovery/register are real and tested.

## Coverage and gaps

- Reviewed personally end to end (the diff is small enough); no subagent
  dispatch this round.
- The dev's live CLI walk of A3–A5 was not re-run by me (the smoke/CLI walk
  needs a pre-provisioned workspace; the claim is consistent with the code
  paths I traced).
- Performance not profiled (no new hot paths beyond the triage scan).
- Contamination note: the main checkout carries uncommitted designer/dev edits
  that predate this review (`M docs/ledger.md`,
  `M docs/plans/plan_high_level_2026-08_webui-polish-e2e.md`, untracked
  `docs/plans/plan_2026-08_supervisor-webui-i31.md` and its high-level twin,
  untracked `.worktrees/`). This reviewer wrote nothing in the main checkout
  except this report; the worktree is clean apart from `node_modules`
  (gitignored, installed to run the web tests).
