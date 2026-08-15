# Review: Fleet Supervisor v2 — fix round (r3)

**Date:** 2026-08-15
**Mode:** diff-scoped re-review of `ac1ff40..11d5ee0` (2 commits, 25 files, +483/−153)
**Verdict:** APPROVE

## Summary

All eight F-findings and the minor batch from the r2 review are fixed with
correct mechanisms, verified by reading every hunk of the round and
cross-checking the call sites. The code is now in good shape: the original
six Criticals and the r2 regression set are closed. Only Minor residuals
remain — none block. This round closes the review.

## Verification performed

- Tests: `cargo test --workspace` → **410 passed**; `npm run test` → **23
  passed** (note: the dev's message claimed 24 — the F-8 cleanup removed the
  reducer test that locked in the dead branch; the count is 23, which is
  expected); `npm run build` clean
- Lint/format: clippy `-D warnings` clean; `cargo fmt --all -- --check` clean
- Every hunk of both commits read and checked against its finding; call sites
  cross-checked on both sides (CLI↔daemon, TS↔serde)

## Fixes verified (all rounds)

**F-1** — `web/src/pages/Graphs.tsx`: `graphNodes(undefined, graphId)` with
`enabled: !!graphId`; the ws-filter regression is gone. **VERIFIED.**

**F-2** — `web/src/components/WorkflowCanvas.tsx:151-165`: removals folded
sequentially into the accumulated graph, one `onChange` per batch; box-delete
now removes all selected nodes. **VERIFIED** (no test — see Minor 3).

**F-3** — `db.rs:279-288` + `journal.rs:46-52`: `std::fs::set_permissions(0o600)`
writes the mode back, on the DB, `-wal`, `-shm`, and the journal (create-mode
kept). The no-op is fixed. **VERIFIED.**

**F-4** — `client.ts` token-change listeners + `app.tsx` re-render on clear:
a 401 now surfaces the missing-token gate immediately; SSE exits without
reconnect; the token state is still module-scoped but reactive. **VERIFIED.**

**F-5** — `supervisor stop` removes the stale pid file on both bail paths
(dead pid, foreign identity). **VERIFIED.**

**F-6** — the smoke-fixture absence is now a recorded deviation in
`2026-08-14-supervisor-wiring-fixes-design.md` (hops asserted 1–4, hop 5
reported; scratch harness deferred with Graph Engine v2). Acceptable
resolution — the deviation is explicit instead of silent. **VERIFIED.**

**F-7** — `inbox.rs`: dead-letter after 5 consecutive failures; later entries
flow; success resets the counter. Both delivery paths (sweep + enqueue) use
the filtered pick. No lock-order hazard (the counter lock is std-mutex,
never held across an await, never nested under the fleet lock). **VERIFIED**
(two nits below).

**F-8** — the dead `""`-keyed `nodeStates` branch and its lock-in test are
removed from `reduce.ts`/`reduce.test.ts`; polling is documented in the
webui spec (line 262-266). **VERIFIED** (one spec-consistency residual below).

**Minor batch (ac5d930)** — all verified: dashboard panic hook restores the
terminal; `supervisor daemon` execs (no orphan on targeted SIGTERM);
`PortSetting` custom deserializer accepts int / `"auto"` / quoted number and
rejects everything else loudly; empty node list rejected at `Workflow::new`;
wrong-typed nested rule values now disable the rule (I-23 residual closed);
`?limit=` (capped 200) replaces the misnamed `since`; permission responses
400 on anything but allow/deny and `remember` is passed through to opencode;
`DELETE /graphs/{id}` actually deactivates with 404 on unknown; cmux handles
fail loudly instead of inventing `surface:0`; the SSE observer gets a
dedicated no-total-timeout client (120s/30s severing gone) and a true
"no-heartbeat-for-90s" deadline (`sleep_until`, not a fresh sleep per chunk);
the supervisor-workspace child gets SIGTERM→10s→SIGKILL; `upsert_workspace`
persists `server_pid`; `parseGraph` no longer crashes on malformed data; the
match-fallback applies to the first consuming graph only (I-4 residual).

## Findings (Minor)

**M-1 — Dead-lettered entries log `error!` every 2s sweep and the failure
map never shrinks.** `services/inbox.rs`: the skip closure logs per
encounter, so a dead-lettered entry re-logs every sweep forever, and
`failed_deliveries` retains every failed entry id for the daemon's lifetime
(including entries that later delivered successfully elsewhere are removed —
only permanently-skipped ids accumulate; bounded by distinct entries, still
worth pruning). Log once per entry per sweep-interval-epoch, or drop to
`warn` after the first notification, and prune the map on sweep.

**M-2 — `missing_role` nodes are invisible in the web UI, and the webui spec
still documents SSE-driven behaviors that no longer exist.** A missing-role
node holds at `ready` in `node_state` (no marker is persisted), the daemon's
`WorkflowEvent::MissingRole` publish has no consumer since the reducer arms
were removed, and the REST poll shows `ready` forever. The webui spec's
§5.1 triage list ("every node in `needs_decision` / `failed` /
`missing_role` — clickable") and §6.2-6.4 (`missing_role` ⚠ glyph,
`loop_back` dashed edges, reducer transitions) describe a mechanism the
polling deviation (recorded at spec line 262) does not provide. Fold into
the I-31 build-or-strike escalation: either persist a pollable marker (e.g.
node state `needs_decision` with the missing role recorded) or strike those
spec lines.

**M-3 — The canvas delete path still has no component test, and it has
regressed twice (C-6, then F-2).** The F-2 fold is 6 lines over the tested
`removeNode`, which is why this is Minor — but a one-file Vitest component
test (Backspace delete → parent graph loses the node; box-delete → all
selected nodes leave) is cheap insurance against a third round.

**M-4 — Claim correction.** Web tests are 23, not 24 (F-8 removed the
lock-in test). Mechanical state is otherwise exactly as claimed.

## What is correct (round 3)

Every fix in the round is present and correct; no new regressions found in
the touched code (checked both sides of each contract: CLI↔daemon stop/exec
pairing, TS↔serde shapes, inbox lock ordering, SSE deadline semantics,
permission `remember` threading end to end). The three rounds leave the
merged supervisor with all six Criticals and all 34 Importants closed, the
bridge deferral and smoke deviation recorded in the specs, and a clean
mechanical state (410 + 23 tests, clippy, fmt, build).

## Coverage and gaps

The round is small enough that personal verification covered every hunk; no
subagent dispatch was needed. Not re-checked this round (unchanged since
r2): live smoke chain (fixture deferred, F-6), live cmux invocation, and
performance profiling of the DB/API paths.
