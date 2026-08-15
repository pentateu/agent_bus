# Review: Fleet Supervisor v2 merged to main

**Date:** 2026-08-15
**Mode:** diff-scoped (merge `33687b8..3d16eb4`, split by component — 9 commits, ~17k lines of new Rust + ~1.7k lines of new web code)
**Range:** `33687b8..3d16eb4` on `main` (merge of `feature/supervisor-orchestration-v2`)
**Verdict:** BLOCK

## Summary

The merge delivers the Fleet Supervisor v2 nearly whole: `supervisor-core` is a
clean, pure, well-tested engine; `supervisor-daemon` implements the external
contracts (opencode 1.18, cmux) correctly and the F1–F6/M1–M10 wiring fixes all
landed as specified; the web UI's API/type fidelity is solid. However, the
review found six Criticals — an unauthenticated path traversal that serves
`api-token`/`secrets.json`/arbitrary files to any local process, three tables
that are written without journal entries and wiped on every restart, a
broadcast-`Lagged` path that silently kills all six service loops, a
`loop_back` gap in the shipped graph, a CLI that never emits its documented
exit codes, and a DAG-editor node deletion that silently fails — plus ~34
Important findings. The Graph Engine v2 design (next cycle) was reviewed
separately and is out of scope here.

## Verification performed

- Build: `cargo test --workspace` → **446 passed** (matches the dev's claim)
- Web: `cd web && npm run test` → **15 passed**; `npm run build` → clean
- Lint: `cargo clippy --workspace --all-targets -- -D warnings` → clean
- Format: `cargo fmt --all -- --check` → clean
- Ran (by reviewers, read-only): built daemon exploited live for the traversal
  (both the transport and security reviewers, independently); CLI exit codes
  and `supervisor stop` pid-reuse reproduced empirically; opencode 1.18.18 wire
  shapes verified live (SSE data-only frames, `/session/status` idle omission).
- Live 3-node `feature_lifecycle` run: **not re-run** — the `smoke` harness
  requires a pre-provisioned workspace fixture it does not create, and it has a
  false-pass defect (Finding I-11) that weakens the prior run's evidence.

## Findings

### Critical

**C-1 — Unauthenticated path traversal serves `api-token`, `secrets.json`, and arbitrary files.**
`crates/supervisor-daemon/src/api.rs:119-132` (`spa` handler). The `/ui/{*path}`
routes are registered without the auth layer (`route_layer` applies only to the
merged `/api/v1` router, `api.rs:99-102`), and `spa` does `ui_dir.join(&path)`
with no `..` rejection, no canonicalization, no prefix check — then reads and
serves the file. Exploited live by two independent reviewers against the built
binary: `curl --path-as-is http://127.0.0.1:<port>/ui/../api-token` returns the
full bearer token; `/ui/../secrets.json` returns `OPENCODE_SERVER_PASSWORD`;
percent-encoded `%2e%2e%2f` works too; an absolute path replaces the base and
serves `/etc/hosts`. Any local process (including another local user's) can read
every file the victim can read and then drive the whole fleet. The browser
vector is blocked only by the absence of CORS headers. Fix: reject `..`/NUL
segments and absolute paths, canonicalize and assert the resolved path stays
under the UI root, or serve `/ui` with auth/allowlist. Verified personally:
code read; two live reproductions.

**C-2 — `proposal`, `intake`, and `usage` are written without journal entries and wiped on every restart.**
`crates/supervisor-daemon/src/state.rs:283-300` (`upsert_proposal`,
`insert_intake`, `insert_usage`) mutate memory + SQLite with no
`journal.append`; `Store::rebuild` (`db.rs:266-287`) drops all tables and is run
unconditionally by `Fleet::open` (`state.rs:78`). §3.2's non-negotiable ("the
DB is never written without a matching journal entry first") is violated, and
§4.13's "proposals survive restarts" is false. Every daemon restart silently
erases intake history, usage/cost data, and pending bake-back proposals.
Verified personally: code read; empirically — a reviewer's daemon start
rebuilt the production DB and the tables came back empty (see disclosure).

**C-3 — A broadcast `Lagged` error permanently kills every service loop, silently.**
`crates/supervisor-daemon/src/services/{inbox,workflow,agent_state,rules,ingest,usage}.rs`
(all six: `Err(_) => return` on `rx.recv()`; e.g. `inbox.rs:53-54`). The bus
doc (`bus.rs:5-6`) says a slow subscriber "must resync from the store" — none
do; they exit with no log. The driver HTTP clients have no timeout
(`clients/opencode.rs:92-95`, default reqwest), so `deliver()` can block
indefinitely on a hung `opencode serve` while SSE frames from other workspaces
fill the 4096-slot ring; when the ring wraps, the service dies forever. No
delivery, no ACK resolution, no timeouts until a manual daemon restart. Fix: on
`Lagged`, warn and continue (or re-subscribe); only `Closed` terminates; add
explicit HTTP timeouts. Verified personally: code read.

**C-4 — `loop_back` on a `big` revision skips the agent-review re-run in the shipped graph.**
`crates/supervisor-core/src/dag.rs:669-686` (`loop_back` resets only
`Ready | Running` downstream nodes; already-`Done` intermediates stay `Done`).
§4.9/§4.11 require `approved:false + needs_revision:big` → "loop back to the
pre-review node (re-run agent review)" before the human sees it again. In the
shipped `feature_lifecycle` (`graphs.rs:21-27`), `hl_human_gate.loop_back.big =
"high_level_design"`: with all three nodes done, a big-revision ACK re-readies
`high_level_design` but leaves `hl_agent_review` Done, so the redesign's ACK
immediately re-readies the gate — the human reviews a redesign that was never
agent-reviewed. The existing test covers only the 2-node case. Fix: reset every
strictly-downstream `Done` node to `Pending`; add a test on the shipped graph
shape. Verified personally: code read.

**C-5 — The CLI never emits the documented exit codes (1 usage / 2 not found / 3 unreachable).**
`crates/supervisor-cli/src/main.rs:158-167` maps every error to exit 1;
`exit_unreachable()`'s `ExitCode::from(3)` is never used (all callers convert to
`anyhow` via `?`); `client.rs:213-223` (`parse`) turns 404s into plain errors;
clap's default usage-error exit is 2, not 1. Reproduced empirically: daemon
down → EXIT=1 (spec 3); 404 → EXIT=1 (spec 2); missing arg → EXIT=2 (spec 1).
The supervisor agent's slash commands branch on these codes to self-heal; they
cannot. Fix: a typed error enum mapped in `main`; detect connect-refused → 3,
404 → 2; print clap errors with exit 1. Verified personally: code read +
reproducer.

**C-6 — Deleting a node in the DAG editor silently does not delete it.**
`web/src/components/WorkflowCanvas.tsx:132-136,160`. Backspace removes the node
from React Flow's internal state only (`onNodesChange` never propagates to
`onChange`); the next structure change re-seeds from the graph and the node
reappears; **save persists the "deleted" node**. The spec's "add/remove nodes"
(§5.3/U4) is not implemented and the failure is invisible. Fix: handle `remove`
changes and call `onChange(removeNode(graph, id))` — `removeNode` already
prunes `depends_on` (`graph-edit.ts:81-88`, tested). Verified personally: code
read.

### Important

**I-1 — Node state is keyed `(graph, node)` without a workspace; two workspaces running the same graph corrupt each other.**
`crates/supervisor-daemon/src/state.rs:218-225`, `services/workflow.rs:118-121` (restore reads `node_states(graph)` for every ws). ws-A's `bug_flow/fix` done overwrites ws-B's row; after a restart B's mid-flight node silently completes without its turn ever being delivered. Note: the §3.1 schema itself has this keying, so the spec shares the flaw — the code matches the spec, and the spec needs a `workspace_id` column. Two reviewers (services + CLI smoke) corroborated.

**I-2 — `on()` / `off()` have no mutual exclusion; a stale `on()` can flip a drained workspace back to On.**
`services/workspace.rs:117-283`. Both handlers interleave freely at awaits (health wait up to 30s, cmux calls); an `on()` started mid-drain can journal `On` after `off()` killed the server and closed cmux. Final state: On with a dead server, no child, inbox already drained into dead sessions. Fix: per-workspace mutex (or transitioning flag) across the whole sequence.

**I-3 — `start_graph` dedupe is check-then-act across awaits → double start.**
`services/workflow.rs:221-255`. Two concurrent starts both pass the guard, both journal `workflow.start` and enqueue — the agent runs the node twice. Fix: hold the instances lock across check+insert.

**I-4 — ACK matching is by bare `task_id`; one ACK completes running nodes in every graph sharing the agent.**
`services/workflow.rs:315-340` applies the resolved ack to *every* graph in the agent's task set; `core/dag.rs:492-533` matches `task_id` only. Two user graphs with the same ack string (the DAG editor encourages custom graphs) → one turn completes both. Stock graphs have disjoint acks, so it needs user graphs. Fix: namespace acks per graph or match against the recorded running task.

**I-5 — `driver = "cmux"` agents can never receive inbox deliveries — silently, forever.**
`clients/registry.rs:56-61` bails with "requires a recorded pane (M9)" for every cmux agent; the inbox sweep retries every 2s forever with no error surfaced. §6.1 documents `driver = "cmux"` as a valid roster config (its own example has `reviewer_cmux`). Either implement the pane-resolved driver, fail bring-up loudly, or mark entries undeliverable.

**I-6 — `off()` never SIGTERMs (immediate SIGKILL) and never kills adopted servers.**
`services/workspace.rs:621-641`. `kill_server` calls `start_kill()` (SIGKILL immediately — the doc comment claims SIGTERM-then-SIGKILL, which is not what happens), and adopted servers are never in `children`, so for a workspace adopted after a daemon restart the kill is a no-op: `off()` journals `off` while `opencode serve` keeps running and holds the port. Fix: real SIGTERM→grace→SIGKILL; kill adopted servers via recorded PID/port (like `shutdown()` does).

**I-7 — The §4.2 bind probe does not exist; instead, bringing up a workspace kills whatever holds its port.**
`services/workspace.rs:188-193,420-430` (`release_port_occupant` on the Kill branch for unrecorded ports). Two projects fixing `port = 4101` → `supervisor on project2` silently SIGKILLs project1's live server; an unrelated local server in the allocator range is killed too. The spec says: probe, and pick another port (for non-recorded workspaces). Fix: implement the bind-then-drop probe before spawn; reserve kill-occupant for recorded-port adopt-or-kill.

**I-8 — Basic auth is documented (§4.16) but not implemented.**
`api.rs:149-166` accepts only `Authorization: Bearer`; zero occurrences of Basic in the file. Any client following the spec gets 401. Implement it or amend the spec.

**I-9 — Manager escalation gives up after ~6s and has no regex/re-ask fallback.**
`clients/manager.rs:150-168`: 1.5s sleep + 3 polls × 1.5s, then `Ok(None)`. §4.12 requires structured → parsed JSON → regex → re-ask-once; a correct decision arriving at t=8s is discarded and the escalation surfaces "unresolved". In practice most escalations will never be acted. Fix: poll until idle (session status), add regex + one re-ask.

**I-10 — `opencode serve` children inherit the daemon's full environment.**
`services/workspace.rs:434-438`, `main.rs:380-384`: `.env(...)` only adds; no `env_clear()`. A daemon started from a shell with `GITHUB_TOKEN`/`OPENAI_API_KEY` exported hands those to every workspace's agents (tool access → prompt-injection exfiltration). Under launchd the env is minimal, hence Important not Critical. Fix: `env_clear()` + explicit allowlist (`PATH`, `HOME`, `OPENCODE_SERVER_PASSWORD`, `CMUX_SOCKET_*`).

**I-11 — The `smoke` acceptance harness can PASS without a live hop.**
`crates/supervisor-cli/src/main.rs:337-388`. Success = "all node rows are done". Node states persist; `start_graph` no-ops while an instance exists (I-3's guard), and journaled `Done` rows survive restart — so a re-run, or a run after any prior completion, prints `smoke: PASS — the live chain completed end to end` with zero agent work. It also does not create the scratch workspace/background agents the fixes-handoff requires, and hops 3–5 are never individually asserted. This weakens the dev's "live 3-node proven" evidence. Fix: require a fresh instance, assert each hop transition, create the scratch fixture.

**I-12 — `supervisor stop` kills any pid found in a stale pid file.**
`supervisor-cli/src/main.rs:268-285`: only `kill -0` liveness, no identity check. A recycled PID (or an agent's planted file) gets SIGTERMed and `stop` reports success — reproduced by a reviewer who killed a `sleep 300` this way. Fix: verify `ps -o comm=` matches `supervisor-daemon` (or port-4198 ownership) before signaling.

**I-13 — `install` / `agent-install` hardcode `$HOME/.supervisor` and the launchd plist cannot honor `SUPERVISOR_STATE_DIR`.**
`supervisor-cli/src/main.rs:550-571`; `supervisor-daemon/src/launchd.rs:11-45` emits no `EnvironmentVariables`. Under the override, the plist launches the daemon against the default dir while the CLI and plist logs use the override → `stop` reports "daemon not running", assets land in the wrong tree. Reproduced with `HOME=`/`SUPERVISOR_STATE_DIR=` overrides. Fix: use `default_state_dir()` everywhere; emit the env var in the plist.

**I-14 — `Posted.ts` has no `#[serde(default)]`; a new CLI decoding an old resident daemon's reply fails.**
`crates/protocol/src/lib.rs:76-81`. The pre-upgrade daemon keeps serving until idle; the new CLI connects to it, gets `{"type":"posted","id":...}` with no `ts`, and fails serde decode (exit 1). Old CLI vs new daemon is fine (unknown fields ignored). Fix: `#[serde(default)]` or `Option<String>`.

**I-15 — Daemon lifetime contract changed without touching the spec or the guide.**
`crates/daemon/src/sweep.rs:47-53`: parked wait/follow clients now count as activity, `MAX_WAIT_TIMEOUT_SECS` was raised 5400→172800 (`core/src/retention.rs:10-18`), and a `follow` has no bound — one open `follow` makes the daemon immortal. The design spec (`2026-08-06-agent-bus-design.md:49`, "exits after 1.5 hours with all partitions idle") and `cli/src/guide.rs:245` still say 1.5h. Update both (and consider bounding `follow`).

**I-16 — The shutdown-behavior change has no integration test.**
`crates/daemon` has no `tests/` dir; only the pure `should_shutdown` predicate is unit-tested. Nothing verifies the waiter counters defer shutdown, return to zero, or that a `u64::MAX` wire timeout still clamps. Add an integration test with a short idle override.

**I-17 — `wait --timeout 0s` regressed from exit 2 (documented "timed out") to exit 1.**
`crates/cli/src/commands/wait.rs:26-33`: a zero budget short-circuits locally with "budget expired before the daemon answered" — the daemon is never asked. Scripts branching on exit 2 for "nothing pending" break. Fix: send `timeout_secs: Some(0)` as one round-trip.

**I-18 — 1,610 lines of dead modules shipped in the production bus core.**
`crates/core/src/{dag,rules,state}.rs` (plus `CoreError::InvalidWorkflow` and a `toml` dependency) implement the superseded `2026-08-10-orchestration.md` design. Verified: zero consumers of `agent_bus_core::dag|rules|state` anywhere in the repo; `supervisor-core` re-implements the same concepts per the current spec; `supervisor-daemon` declares `agent-bus-core` but uses nothing from it. Two `Workflow` types and two rule engines exported from public crates is a divergence hazard. Fix: move them out of the published core or delete.

**I-19 — `ingest` converts invalid JSON payloads into `null` and reports success.**
`supervisor-cli/src/client.rs:203-209`: `serde_json::from_str(payload).unwrap_or_default()`. A typo'd payload creates a real intake row with empty title/body and `"queued": true`. Fix: propagate the parse error.

**I-20 — `dag status <unknown>` exits 0 silently; bare `bake-back` exits 0 doing nothing.**
`supervisor-cli/src/main.rs:501-520,441-481`. `dag status typo && proceed` proceeds as if the graph exists; a cron'd `supervisor bake-back` silently never generates proposals. Fix: exit 2 on unknown id; `ArgGroup` requiring one of `--preview/--apply/--reject`.

**I-21 — `status` shows no queue depth, though §4.15 and the CLI's own doc require it.**
No inbox-depth endpoint exists in `api.rs`; `status` prints only workspace/agent rows. Add the field to the API payload and the column, or amend the spec.

**I-22 — Bake-back embeds the wrong confidence in the proposed rule TOML.**
`supervisor-core/src/bakeback.rs:115-120`: `generate_rule_toml` ignores `_cluster_size` and computes the confidence from the single representative decision (so it is 1.0 or 0.6), while `Proposal.confidence` is the cluster rate. A 0.25-success cluster produces a rule that always clears the 0.8 threshold. Fix: pass the cluster rate through.

**I-23 — Unknown keys nested under the `agent` table are silently dropped; a typo'd rule matches everything.**
`supervisor-core/src/rules.rs:364-373`: the `"agent"` table arm reads only `role`/`type` and pushes nothing to `unknown_keys`. `when = { agent = { r0le = "tester" } }` yields no constraint at all — the exact "typo must not silently match everything" failure §4.10 forbids. Fix: push unrecognized nested keys into `unknown_keys`.

**I-24 — The SSE stream is not abortable; unmount leaks a permanent zombie connection.**
`web/src/api/sse.ts:55-93` has no `AbortSignal` and the generator's `for(;;)` reconnect survives `return()`; `live-store.tsx:19-31` only flags `cancelled`. Every dev StrictMode/HMR remount leaves the previous connection consuming keep-alives forever, reconnecting on daemon restarts. Fix: wire an `AbortSignal` into `fetch` and `reader.cancel()` in a `finally`.

**I-25 — No 401 recovery: a rotated token renders a misleading empty dashboard.**
`web/src/api/client.ts:44`, `app.tsx:34`, `Dashboard.tsx:146-150`. After a token rotation every call 401s and the app shows "No workspaces yet — run `supervisor add`" — actively wrong advice. Fix: clear the token on 401 and show the missing-token screen; stop SSE reconnects.

**I-26 — Live canvases poll REST every 2s; the reducer's SSE node-state machinery is dead code, and `loop_back`/`missing_role` events are dropped.**
`web/src/pages/{Dashboard,Graphs}.tsx` use `refetchInterval: 2000`; `store/reduce.ts:75-89` node states are keyed under `""` and have zero consumers; `default: return null` drops `loop_back` and `missing_role` wire events. Root cause is real: workflow events carry no `workspace_id` (`core/dag.rs:229-245`), so graph→workspace keying is ambiguous. Fix: emit `workspace_id` on workflow events and consume them, or document polling as the mechanism (spec §5.1/§6.4 say "nothing polls").

**I-27 — Permission banner never clears.**
`web/src/store/reduce.ts:90-100` sets `permissionPending` and nothing ever clears it (no resolved signal exists in `signal.rs`); after Allow/Deny the stale banner and pid persist across navigation. Fix: clear on mutation success.

**I-28 — Mutation failures have zero user feedback.**
`Graphs.tsx:29-32` (`void save()`), `Dashboard.tsx:85-86`, `Agent.tsx:74-79`: unhandled rejections on save/on/off/abort/attach. A daemon restart mid-edit → click save → nothing happens, graph not persisted. Fix: try/catch + inline error rendering.

**I-29 — TS `usage` type declares snake_case; the wire is camelCase.**
`web/src/api/types.ts:125` (`prompt_tokens`/`completion_tokens`) vs `clients/driver.rs:52-57` (`#[serde(rename_all = "camelCase")]` → `promptTokens`/`completionTokens`). Latent `undefined` for any future token display.

**I-30 — Missing tests for real logic.**
Zero tests for: `parseSseFrames`/`frameToBusEvent` (chunk boundaries, keep-alive comments), `bootstrapToken`, API client error paths, WorkflowCanvas render/interaction (spec §7 requires them), `layout.ts`, reconnection/backoff. These are exactly the seams where C-6/I-24/I-25 live.

**I-31 — Large set of spec'd UI features is absent.**
Per `2026-08-14-supervisor-webui-detailed-design.md`: node-level triage (§5.1), workspace foreground/background filter + cost mini-chart + resume control (§5.2), agent activity feed + decide banner (§5.4), property panel fields `agent_id`/`done_when.approved`/`match`/`on_error`/`gate`/`loop_back`/`timeout_secs` (§5.3), rules list/TOML editor (§5.5), intake page (U6), loop_back dashed edges + ✓/✕/⛔ glyphs (§6.2-6.3). Also every installed graph gets a permanent canvas (§5.1 says "for each *running* graph"). Either build or strike from the spec.

**I-32 — `journal.jsonl`, `supervisor.db`, `fleet.json` are 0644 secret sinks.**
`journal.rs:40-44` and `db.rs` create files with default umask; inbox bodies (which contain pasted tokens) are journaled verbatim (`state.rs:182-192`). Verified on disk: `-rw-r--r--` inside a 0755 `~/.supervisor`. Fix: 0600 files, 0700 state dir.

**I-33 — `supervisor web` prints the bearer token to the terminal and passes it in argv.**
`supervisor-cli/src/main.rs:535-540`: `println!("opened {url}")` with `#token=<full token>` — captured by scrollback, screen recording, wrapper logs. Fix: print the URL without the fragment.

**I-34 — The agent-bus bridge (§7.3) is not implemented.**
No bridge worker (outbound `post` / inbound `wait` per partition) exists anywhere in the supervisor crates (verified by grep; `supervisor-daemon` never invokes the agent-bus binary or bus core). §7.3 is the mechanism that closes the loop for non-opencode harnesses. Either implement it or record the deferral explicitly in the spec.

### Minor

Grouped by theme; each verified by a reviewer:

- **Core:** quoted `port = "4200"` parses as `Auto` and is silently ignored (`config.rs:19-23,117-123`); empty node list parses and reports instantly complete (`dag.rs:284-336`); data-vs-code tie-break relies on an unenforced `code:` id prefix (`rules.rs:709-711`); §8 `recovery`/`StepEnded` deviations are not recorded in the spec (§13 style); ACK layer 2 accepts a JSON ack anywhere in the output, not only as the final object (`ack.rs:109-117`).
- **Daemon services:** `FleetEvent::AgentState` publishes have no consumer; `AgentStateTracker::apply` TOCTOU on transition re-validation; `Fleet::free_port`/`PortFree` journal type dead; `off()` leaves `session_index`/`panes` entries (harmless, verified); inbox retry storm (2s) for permanently-failing entries with no dead-letter.
- **Daemon transport:** heartbeat watchdog resets on any chunk, so "90s no heartbeat" is really "90s total silence" (`sse.rs:218-226`); trailing partial SSE line dropped on clean EOF (`sse.rs:241-244`); `upsert_workspace` never writes `server_pid` (DB projection not faithful); permission `response` values other than "allow" silently deny and `remember` is dropped before the opencode call; `DELETE /graphs/{id}` returns 200 `{"deleted": false}`; `agent_messages` treats `since` as a limit; `write_secure` writes-then-chmods; cmux `extract_handle` falls back to bogus `surface:0`; supervisor-workspace child is SIGKILLed with no grace (`main.rs:304-306`).
- **Bus:** nanosecond race where the sweep can shut down under a just-parked waiter (self-heals via reconnect, `sweep.rs:45-52` vs `server.rs:232`); `post`/`status` output shapes changed (additive JSON, but text parsers break); daemon log has no rotation/levels (`logging.rs:56-70`).
- **CLI:** `stop`'s timeout message says 30s when the deadline is 60s and hardcodes `~/.supervisor/daemon.log`; `daemon` subcommand forwards no signals (targeted SIGTERM orphans the child); dashboard has no panic hook (raw terminal on panic); dashboard freezes up to 3 minutes per refresh on a hung daemon (client-wide 3-min timeout).
- **Web:** React pinned to 18.3.1 (standards say 19; spec table says 18 — stale either way); `?limit=` is ignored by the backend (`since` semantics); no ESLint/Prettier; unguarded `JSON.parse` on graph data; `useGraphNodeStates(id ?? "")` fires `GET /graphs//nodes` on the list page; `connect` doc comment inverts direction; a11y (color-only node state, tabs lack arrow keys, placeholder-as-label, index-keyed transcript rows).

## What is correct

- **supervisor-core purity:** no I/O/async/tokio deps; `#![forbid(unsafe_code)]`; `unwrap` only in tests (one compile-time-constant `expect` with a targeted allow); `thiserror`.
- **Port allocator:** lowest-free-first, reserved ports never handed out, reserve/free semantics correct, tested incl. exhaustion.
- **ACK resolver:** strict layered precedence (structured → text-JSON → regex → match → none) implemented and tested; human-gate `approved`/`needs_revision` parsing in both JSON and regex paths.
- **DAG core:** cycle detection (incl. self-loops) rejects at parse; unknown deps, duplicate ids, dangling `loop_back` targets rejected; readiness fixpoint; rerun bounds enforced; gate completes only on `approved:true`; missing-role holds at ready and is surfaced.
- **Rule engine:** all §4.10 operators exercised; confidence cascade with threshold; top-level unknown-key disabling works; counter store is panic-free.
- **Journal replay:** idempotent full-state records; torn/corrupt lines skipped with line numbers; sequence recovery verified.
- **All F1–F6 / M1–M10 landed as specified** — delivery-on-enqueue (F1), WorkspaceState publish (F2), start endpoint + ingest wiring (F3), command dispatcher + running_task + escalate path (F4), supervisor workspace with adopt-or-kill + knob (F5), bake-back triggers (F6), M4/M5/M6/M8/M9/M10, workflow restart `Running → Ready` (M3), SSE cached session-map resolver (M7, sound — stale remap impossible), StepEnded no longer maps to Idle (M2).
- **The two historical bugs are genuinely fixed:** usage-collector deadlock (guard dropped before re-acquisition; regression test present) and the lost-idle-signal `fleet.try_lock()` hazard.
- **External contracts:** opencode endpoints/methods per §7.1, no nonexistent `serve` flags (verified live against opencode 1.18.18); cmux arg vectors per §7.2 (`new-surface` without `--command`, `send` + Enter for attach, no shell-quoting hazard); SSE mapping respects sessionID-less frames (heartbeat never maps to an agent), unknown events ignored, backoff 1–60s with reset.
- **Adopt-or-kill:** PID match AND health check before adopting; recorded ports never switched; health wait ≤30s; resume re-validates session ids.
- **API surface:** all §4.16 routes present with correct methods; loopback-only bind (verified); bearer auth enforced on every `/api/v1` route including `/events`; malformed JSON rejected; handler panics cannot kill the daemon.
- **Bus wire compat (both directions):** old journal records parse (serde defaults + golden test); new Instruction messages serialize byte-identically to base; the `u64::MAX`-timeout overflow clamp is retained; RAII waiter counters cannot leak or underflow (every path audited); no bodies ever logged.
- **Web API/type fidelity:** every REST path/method/body matches a daemon route; response field names match serde shapes (except I-29); SSE Bearer header on fetch-stream, no query-token, token in memory only, hash stripped via `replaceState`; no CORS headers set (browser read vector blocked); reducer purity; graph-edit prune/validate logic correct and tested.
- **Mechanical:** 446 Rust tests + 15 web tests pass; clippy `-D warnings` clean; fmt clean.

## Coverage and gaps

Reviewed by seven specialists in parallel: supervisor-core correctness (DONE_WITH_CONCERNS), daemon services concurrency (DONE_WITH_CONCERNS), daemon transport/API/DB/clients (DONE_WITH_CONCERNS), bus-crate regression (DONE_WITH_CONCERNS), supervisor-cli contract (DONE_WITH_CONCERNS), web UI (DONE_WITH_CONCERNS), security (DONE_WITH_CONCERNS). No reviewer was BLOCKED.

Not reviewed / gaps stated honestly:

- **Performance:** no dedicated pass on `db.rs` access patterns (single `rusqlite` connection behind a mutex — matches spec decision §13.5, but the API hot paths and usage-collector frequency were not profiled).
- **Live smoke re-run:** not performed (fixture requirement + false-pass defect, see I-11).
- **Live cmux integration:** arg vectors code-verified against §7.2; no real cmux invocation.
- **launchd:** plist reviewed statically; not loaded live.
- **Vendored `.opencode/skills/**` content in the merge:** out of scope (bundled tooling).
- **Supervisor-agent slash commands (C13):** the CLI surface they call was reviewed; the opencode command definitions themselves were not exercised.

## Design alignment

Conforms: crate purity boundaries, port reservation (4198/4199), ACK layering, adopt-or-kill, SSE observer mapping, journal model and single-authority rule (except C-2's three tables), config formats §6.1/§6.2, command surface §4.15 (except exit codes), API route table §4.16 (except basic auth), launchd shape, web token bootstrap, and the §13 deviations (state path, StepEnded) which are recorded or justified.

Deviates (all unrecorded in the specs — each needs either a fix or an explicit §13-style record): C-2 (journal-first for proposal/intake/usage), C-5 (exit codes), I-6 (SIGTERM sequence), I-7 (bind probe), I-8 (basic auth), I-9 (manager fallback depth), I-15 (bus daemon lifetime), I-18 (superseded engines exported from bus core), I-21 (status queue depth), I-34 (bridge worker absent), I-31 (web UI feature set). The node-state keying (I-1) is shared with the §3.1 schema itself.

## Disclosure

During verification, one reviewer started the daemon with a typo'd
`SUPERVIISOR_STATE_DIR` assignment, causing `Fleet::open` to rebuild the real
`~/.supervisor/supervisor.db` (the journal was intact and replayed; `intake`,
`usage`, and `proposal` rows were dropped — live confirmation of C-2), and ran
`pkill` on supervisor-daemon/opencode processes (none were running; a stale
pid file remains). No repository files were modified; the working tree is
clean, which this report's author verified at the end of the review.
