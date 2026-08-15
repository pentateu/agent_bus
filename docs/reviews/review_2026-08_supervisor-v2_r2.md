# Review: Fleet Supervisor v2 — fix round (r2)

**Date:** 2026-08-15
**Mode:** diff-scoped re-review of the fix round `3d16eb4..ac1ff40` (14 commits, 50 files, +1607/−1843)
**Verdict:** APPROVE WITH CHANGES

## Summary

All six Criticals from the first review are fixed with correct mechanisms, and
the majority of the 34 Importants are fixed as claimed. Two fixes introduced
new problems (a web regression where the Graphs page's live canvas always
shows empty node states, and a batch-delete regression in the DAG editor), and
two fixes are incomplete (the DB/journal file-mode fix is a no-op due to an
`set_mode`-on-a-copy API mistake, and the 401-token-clear is non-reactive).
These are Important-class; nothing reopens the original BLOCK. Fix list below,
then this is mergeable.

## Verification performed

- Build/tests: `cargo test --workspace` → **410 passed** (446 → 410 fully
  explained: −42 tests from the removed dead bus-core modules + 6 new tests);
  `npm run test` → **24 passed** (up from 15); `npm run build` clean
- Lint/format: clippy `-D warnings` clean; `cargo fmt --all -- --check` clean
- Fix verification: personal code review of C-1..C-5, I-8/I-10/I-13/I-14/
  I-15/I-16/I-17/I-18/I-21/I-33/I-34 + two adversarial verifier dispatches
  (Rust cluster: I-1..I-7, I-9, I-11, I-12; web/security cluster: C-6,
  I-19/I-20, I-22..I-30, I-32)

## Criticals — all fixed

| # | Fix | Verified |
|---|---|---|
| C-1 path traversal | `..`/NUL segments rejected; canonicalize + prefix check | VERIFIED (code read; mechanism sound) |
| C-2 journal-first for proposal/intake/usage | `ProposalRecord`/`IntakeRecord`/`UsageRecord` journal types + replay arms; intake upsert-by-id on replay; usage restored via `Store::apply` | VERIFIED |
| C-3 Lagged kills services | `recv_or_shutdown` warns + resyncs on `Lagged` in all 6 services; opencode client 5s connect/30s total timeouts; SSE stream gets a dedicated 120s cap | VERIFIED |
| C-4 loop_back skips re-review | all strictly-downstream nodes (incl. `Done`) reset to `Pending`; new test on the shipped `feature_lifecycle` shape | VERIFIED |
| C-5 exit codes | typed errors (`DaemonUnreachable`→3, `TargetNotFound`→2, `ApiFailure` 404→2), `try_parse`→1, `exit_code()` chain-walk | VERIFIED |
| C-6 editor node delete | single-node delete propagates to parent + save | VERIFIED — **but batch delete regressed, see F-2** |

## Important — fixed and verified

I-1 workspace-keyed node state (schema + queries + replay; legacy records
replay under an empty-ws key — archived, not corrupting; all callers updated);
I-2 on/off per-workspace lifecycle mutex held across every await, no deadlock,
resume/ingest honor it; I-3 start dedupe check+insert atomic; I-4 ACK bleed
closed by first-consumer task-scoped dispatch; I-6 real SIGTERM→10s→SIGKILL +
adopted servers killed via recorded port; I-7 bind-then-drop probe before
spawn, occupant-kill restricted to recorded ports; I-9 manager escalation now
layered (structured→JSON→regex) with idle-polling, one re-ask, ~92s ceiling;
I-13 plist emits `EnvironmentVariables`; I-14 `#[serde(default)]` on `ts`;
I-15 spec updated; I-16 daemon shutdown integration test exists and passes;
I-17 `--timeout 0` still asks the daemon once (exit 2); I-18 dead bus-core
modules deleted (−1630 lines); I-19 ingest JSON errors propagate; I-20
`dag status` exits 2, bare `bake-back` bails with usage; I-21 `inbox_depth` in
API + `status` column; I-22 cluster confidence threaded into the TOML; I-23
nested unknown keys disable the rule (tested); I-24 SSE abortable, StrictMode
leaves one connection, no reconnect after abort; I-27 permission banner clears
on resolve; I-28 mutation errors surface inline; I-29 usage types camelCase;
I-30 9 new SSE-parser tests; I-33 web prints base URL only; I-34 bridge
deferral recorded in the spec; I-10 `env_clear` + allowlist on both spawn
sites; I-32 `fleet.json` tmp 0600 + state dir 0700 (partial — see F-4).

## Findings — fix round

### Important

**F-1 — Graphs page live canvas always renders empty node states (new regression from I-1).**
`web/src/pages/Graphs.tsx:13`: `api.graphNodes(graphId ?? undefined, graphId ?? "")` sends the graph id as the new `ws` filter, so the daemon returns zero rows and the canvas never colors Running/Done (silent, 2s poll). Should be `api.graphNodes(undefined, graphId)`. Verified personally + by verifier.

**F-2 — Batch node delete drops all but the last node (new regression from C-6).**
`web/src/components/WorkflowCanvas.tsx:147-155`: each `remove` change rebuilds from the same stale `graph` prop; React batches the `setEdit`s → last one wins. Box-select 3 nodes → Delete → two resurrect and get saved. Fix: fold all removals in the batch into one `onChange`, or apply sequentially to the accumulated graph.

**F-3 — The 0600 file-mode fix for the DB and the journal re-assert is a no-op.**
`crates/supervisor-daemon/src/db.rs:280-283` and `journal.rs:49-51`: `metadata.permissions().set_mode(0o600)` mutates a copy — it never writes the file back. The DB (rusqlite `Connection::open`, no mode control) is still 0644 on fresh installs; `-wal`/`-shm` are never addressed; a pre-existing 0644 journal stays 0644 (only the `.mode(0o600)` on create protects new journals). The round-1 I-32 secret-sink finding is therefore only half fixed. Fix: `std::fs::set_permissions(path, Permissions::from_mode(0o600))` after open, for the DB and both sidecar files; keep the journal create-mode.

**F-4 — 401 token-clear is non-reactive: a rotated token leaves a dead dashboard.**
`web/src/api/client.ts:44-50` + `app.tsx:34-44`: `setToken(null)` mutates a module-scope variable with no re-render; the missing-token gate only appears on the next Shell re-render (an SSE event or a nav click). REST queries fail silently and SSE has exited. Fix: make the token state reactive (or force a Shell re-render) on clear; consider a manual re-bootstrap affordance.

**F-5 — `supervisor stop` refuses a stale pid but never removes the stale file.**
`crates/supervisor-cli/src/main.rs:307-320`: after the identity bail, `supervisor.pid` stays — every subsequent `stop` re-fails identically until manual cleanup. Remove the stale file on the bail path.

**F-6 — `smoke` still does not create the scratch workspace/background agents the fixes-handoff requires, and hop 5 (next node Ready) is never asserted.**
The false-pass itself is fixed (`saw_running` gate + `already_running` refusal). The fixture requirement remains: the harness operates on a caller-supplied workspace, so a fresh machine cannot run the acceptance chain without manual setup. Hop 5 is cosmetic. Either build the scratch fixture or record the deviation in the handoff doc.

**F-7 — cmux head-of-queue retry storm still blocks every entry behind a permanently-failing delivery.**
I-5 was fixed as claimed (warn per failure + bring-up warn), but a permanently-failing entry is retried every 2s forever and blocks the whole queue. Add a dead-letter/mark-undeliverable after N failures, or surface the failure in `status`.

**F-8 — Claim correction for I-26: the resolution was the documented-deviation option, not the claimed wiring.**
Workflow events still carry no `workspace_id`; canvases still poll REST; the spec now documents ~2s polling (commit cabcc5c) — which is the fallback my original finding sanctioned ("or drop the dead reducer branch and document polling"). That is acceptable, but the commit message claimed "web node-state wiring". Two cleanups follow: remove the dead `""`-keyed `nodeStates` branch in `web/src/store/reduce.ts` (and the test at `reduce.test.ts:36-42` that locks in the bug), and either implement `loop_back`/`missing_role` handling in the polling layer or strike them from the spec's UI contract.

### Minor

- SSE stream is severed by the 120s total-request timeout every 2 minutes (reqwest total timeouts cover body reads); the reconnect path absorbs it, but prefer a read-timeout-only client for the stream (`sse.rs:204`).
- I-7 edge: `allocator.free(port)` on a held fixed port can revoke another workspace's dynamic allocation in range (`workspace.rs:156`).
- I-12 identity check is comm-name only; a recycled PID belonging to a different state dir's `supervisor-daemon` would still be signaled.
- I-23 residual: a wrong-typed nested value (`agent = { role = 123 }`) still yields `agent_role = None` without disabling the rule.
- I-4 residual: the `done_when.match` fallback still applies across all of the agent's graphs.
- I-2 `transitions` map is never pruned (bounded by workspace count).
- Pre-existing: `states = nodeStates ?? {}` re-creates `editSeed` every render, re-running the re-seed effect in edit mode.

## What is correct (round 2)

All 6 Criticals fixed with sound mechanisms; 22 of the Importants verified
fixed (list above); mechanical state clean (410 + 24 tests, clippy, fmt,
build); the fix round also deleted 1,630 lines of dead bus-core code; the
test-count drop is fully explained (−42 dead-module tests, +6 new); journal
legacy-record replay is non-crashing and non-corrupting.

## Coverage and gaps

Two adversarial verifier passes covered every claimed fix in the Rust and
web/security clusters; no fix was left unexamined. The live smoke chain was
not re-run (harness still lacks the scratch fixture, F-6). Performance was
out of scope again, as in round 1.
