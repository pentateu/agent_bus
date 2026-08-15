# Review: supervisor graph engine v2 (P1–P7) detailed design

**Date:** 2026-08-15
**Mode:** plan/design review (diff-scoped to `docs/specs/2026-08-14-supervisor-graph-engine-v2.md`)
**Range:** new file, 512 lines
**Verdict:** APPROVE WITH CHANGES

## Summary

The design lifts seven LangGraph ideas (typed state, step checkpointing, interrupts,
conditional routes, subgraphs, fan-out, transition streaming) into the pure
`supervisor-core` engine, phased A2 (P1/P2/P7, now) and B2 (P3/P6/P5/P4, after web UI v1).
The phasing is sensible, the non-goals are explicit, and most claims about the current
code check out (verified below). Five Important gaps must be resolved in the doc before
implementation: a method-name collision, a template-rendering rule that would corrupt the
shipped graphs' prompts, a self-contradictory fan-out completion rule, an unbounded
journal-growth/replay cost, and an undefined resume completion mechanism. Four minor
notes follow.

## Verification performed

- Read the design doc in full and checked every claim against the code. File:line cites
  below are from the current working tree.
- Build/tests not run for this review: the doc is a design, not code; no code changed.

## Findings

### Important

**G1 — `Workflow::state()` accessor name collision (spec §1.1).**
`Workflow` already has `pub fn state(&self, id: &str) -> Option<NodeState>`
(`crates/supervisor-core/src/dag.rs:366`). §1.1 proposes a new accessor
`state() -> &WorkflowState` on the same type. Rust has no method overloading, so this
cannot compile, and existing call sites (`wf.state("brainstorm")`) must keep working.
Rename the new accessor (e.g. `workflow_state()` or `snapshot()`). Also §1.1 says the
types go in "`dag.rs` + new `state.rs`-side types" — `supervisor-core/src/state.rs` is
already the agent-state transition table; state where `WorkflowState`/`NodeAck` actually
live (dag.rs or a new module) so the implementer does not guess.

**G2 — `{key}` rendering rule would corrupt the shipped graphs' prompts (spec §1.1/§1.3).**
§1.1 changes vars to `BTreeMap<String, serde_json::Value>`; §1.3 renders `{key}` as
"JSON stringified". A JSON string stringifies with quotes (`"fix login"`), so the
default graphs — which interpolate `{bug}`, `{feature}`, `{spec}` into prompt prose
(`crates/supervisor-core/src/graphs.rs:13,64`) — would deliver quoted, corrupted text.
Specify the rule: a `Value::String` renders as its bare contents; non-strings stringify.
Extend the §1.4 legacy-vars test to assert the unquoted form.

**G3 — Fan-out parent completion is self-contradictory (spec §7.1).**
§7.1 says the parent's `done_when` "is ignored", that a fan-out node "must not also set
`done_when.ack`", and that dispatch runs "on the parent's completion event (inside
`apply_ack` on the parent)". Three problems: (a) `Workflow::new` rejects any node with
no criterion (`has_criterion()` = ack or match, `dag.rs:294-299`), so the validation
rule must change or the parent must keep `match`; (b) if `done_when.ack` is forbidden,
no `apply_ack` can ever fire for the parent; (c) the join rule ("parent stays Running
until all children complete") implies the engine, not an ack, completes the parent.
Decide and state one mechanism — recommended: the parent keeps a criterion
(ack or match) as the *dispatch trigger*, the engine exempts fan-out parents from the
generic ack-completion path, and the join transitions the parent (`Done` all-done,
`NeedsDecision` on exhausted child). Update the §7.1 validation sentence to match.

**G4 — Journal growth and restore replay cost are unbounded (spec §2, §3).**
P2 journals a full `WorkflowState` snapshot on every transition, and P1 payloads can be
large (typed node outputs). The supervisor journal is append-only with no pruning
(`crates/supervisor-daemon/src/journal.rs` — `rewrite` rebuilds the same records, no
retention), and §3's `restore()` replays every `workflow.step` record per `(ws, graph)`
from the start of the run. Two consequences: the journal file grows forever, and
restore time grows with run length. The §2.1 "10k ring" caps only the in-memory runlog,
not the disk journal. Specify a retention rule before implementation. Options:
- (a) Keep journal-first, add compaction: when a run completes (or on startup),
  `rewrite` the journal keeping only the latest step record per node (plus a bounded
  runlog tail) — preserves the source-of-truth rule; adds a compaction pass.
- (b) Journal steps, then let the DB `rewrite` drop steps older than N per run — same
  pass, simpler rule, loses old runlog.
- (c) Do not journal per-step snapshots at all; journal only the node-state rows
  (today's behavior) and persist the runlog ring as a projection — cheapest, but breaks
  the M3 replay claim in §3.
Recommended: (a), with the runlog ring persisted per `(ws, graph)` at compaction so the
runlog endpoint does not re-scan the whole journal after every restart.

**G5 — P3 "re-evaluates `done_when`" is undefined for ack/match criteria (spec §4.1).**
`DoneWhen` today is `{ ack, approved, match }` (`dag.rs:70-81`); none of these can be
satisfied by an external `value`. §4.1 claims a resume value can "complete immediately"
but does not define the mechanism, and it also cannot, because `has_criterion()`
(dag.rs:294) rejects nodes whose only gate is `input`. Decide between:
- (a) `done_when.input` is itself a completion criterion: resume(value) completes the
  node directly (value recorded in `state.vars[node]`), and `has_criterion()` counts
  `input` — the true `Command(resume)` equivalent; simplest; the "re-deliver a resumed
  message" path applies only when the node also has an ack/match.
- (b) Resume always re-delivers to the agent; completion still requires an ACK; `input`
  only parks/unparks — closer to today's flow but adds a turn per interrupt.
Recommended: (a). Also state who may resume (human, manager, rules — §4.2 implies all
three) and what happens to the parked agent's inbox entry while `WaitingInput`.

### Minor

**G6 — Rewind keeps stale `acks`/`vars` of reset nodes (§2.2).** Fork semantics keep
state, but after rewind the reset node's previous completion remains in
`state.acks[node]` / `state.vars[node]` until a re-run overwrites it; if the re-run
fails, the stale output stays forever and P6 predicates/manager contexts read it. State
the rule: rewind clears (or marks superseded) the reset nodes' acks/vars.

**G7 — Rewind publishes `LoopBack { revision: None }` (§2.2).** Expressible today
(`Revision::None`, `types.rs:130`), but `LoopBack` is the human-gate revision event;
reusing it for rewind conflates two semantics in the event stream and in
`handle_event`'s LoopBack arm (`workflow.rs:535-539`), which re-readies the *target*.
Prefer a dedicated `NodeRewound` event or reword §2.2 to publish only `NodeReady`.

**G8 — Fan-out concurrency is bounded by the roster, not by `max_concurrency` (§7.2).**
Children resolve roles to agents; with one dev agent, N children serialize in that
agent's inbox regardless of `max_concurrency`. Note the effective ceiling
(`min(max_concurrency, matching-role agents)`) so the UI does not promise parallelism
the fleet cannot deliver.

**G9 — §2.1's two alternatives are written as an open choice.** "a new `workflow_step`
table or reuse `journal` — reuse the `journal` table only" reads like a leftover
decision note. Lock it (journal-only is consistent with journal-first) and delete the
table alternative.

## What is correct

- Phasing and gates (§10) are coherent and match the repo state: Phase A wiring fixes
  have landed; web UI v1 is the B2 gate.
- Verified claims: `Workflow::new` validation of duplicate ids, unknown deps, cycles,
  missing criteria, and `loop_back` targets (`dag.rs:284-323`); `reachable_from` exists
  (`dag.rs:730`); `JournalType::WorkflowStart` + `WorkflowStartEvent { ws, graph, vars }`
  with string vars (`journal.rs:28,147-156`) — so §3's "change the payload to
  `WorkflowState`" describes a real, needed change; `FleetState::apply` replay
  (`state.rs:503-577`); `WorkflowRunner::restore()` rebuild + Running→Ready
  (`workflow.rs:105-143`); `persist_node` is the single transition funnel
  (`workflow.rs:548`) — so P7's "publish on every persist_node" is right; `Signal::NeedsInput`
  exists (`signal.rs:48-51`); `Action` lives in `rules.rs:85`; `/api/v1/events` SSE
  exists (`api.rs:94`); CLI `dag` subcommand with `DagAction` (`supervisor-cli/src/main.rs:87-126`);
  `NodeState` serde snake_case (`types.rs:96-114`).
- §12's `Workflow::invariant()` requirement is well placed for a pure engine; the
  "total transition functions, never panic" rule matches existing `apply_*` style.
- §13 non-goals are consistent with the engine's purity and the journal model.

## Coverage and gaps

Single-specialist review (design alignment + spec consistency against the code). Not
covered: web-UI impact beyond §11 (the editor-schema note is correct but shallow —
schedule a UI review when P7/P2 endpoints land), and the LangGraph comparison itself
(taken as the doc's input premise).

## Design alignment

- Journal-first respected: all new state flows through `workflow.step` records.
- Pure-core boundary respected: P1–P7 core changes are pure; runner/API changes stay in
  the daemon.
- Backward compatibility respected via `#[serde(default)]` on new `NodeDef` fields and
  keeping `loop_back`.
- No deviation from the non-negotiables in
  `docs/specs/2026-08-14-supervisor-fixes-handoff.md`.
