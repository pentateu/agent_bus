# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

- **Primary — one developer/operator on one machine (macOS).** Runs a fleet of
  local opencode agents in terminal sessions and cmux panes. The web UI is the
  glance-and-control surface: see what every workspace/agent is doing, triage
  what needs attention, edit workflows, and steer.
- **Secondary — the agents themselves.** The supervisor agent and slash
  commands drive the product through the CLI; they read the same state and
  journal. The manager agent is a programmatic decision engine, never
  human-facing.

## Product Purpose

agent-bus is a local event bus that lets agents in separate terminal sessions
on one machine talk to each other, plus the Fleet Supervisor that owns those
sessions: bring workspaces on/off, run DAG workflows across agents, escalate
and decide failures, bake decisions back into rules, and record every change in
a replayable journal. Success = a workflow runs end-to-end across agents and
the human can see and steer it at a glance.

## Positioning

Local, journaled, self-healing agent fleet with rule-based escalation and
bake-back. Everything runs on one machine; nothing is cloud-hosted; state
survives restarts through the journal.

## Operating Context

- Loopback only (`127.0.0.1:4198`), single user, bearer token, no accounts, no
  public deployment, no CORS.
- Terminal-first culture: the ratatui dashboard stays for quick terminal use;
  the web UI is additive, the rich surface.
- Terminals live in cmux; each supervisor workspace maps to a cmux workspace.
- Project docs are written in ASD-STE100 Simplified Technical English.
- The design authority lives in `docs/specs/` (date-prefixed specs); plans in
  `docs/plans/`; status in `docs/ledger.md`.

## Capabilities and Constraints

- Terminology: workspace, agent, node, graph, workflow, triage, bake-back,
  escalation, decision, proposal, journal, SSE, live canvas.
- The journal is the single source of truth. The DB is never written without a
  matching journal entry first (non-negotiable).
- Web UI token: in memory only, stripped from the URL hash, never persisted; a
  missing token shows "run `supervisor web`".
- Cost figures are estimates, labeled "est.", never billing.
- Required capability (open work): bringing a workspace on must scan cmux and
  adopt an existing cmux workspace/surfaces instead of creating duplicates;
  the supervisor must be able to read terminal state.
- The dashboard represents what is running; idle graphs are not animated
  (decided 2026-08-15, see the I-31 plan).
- Undecided facts: none outstanding at init time; open questions live in the
  plans.

## Brand Commitments

None. Internal tooling; no brand identity, voice, or asset commitments beyond
the names "agent-bus" and "supervisor".

## Evidence on Hand

- Specs: `docs/specs/2026-08-06-agent-bus-design.md`,
  `2026-08-13-supervisor-detailed-design.md`,
  `2026-08-14-supervisor-webui-detailed-design.md`,
  `2026-08-14-supervisor-graph-engine-v2.md`.
- Reviews: `docs/reviews/review_2026-08_supervisor-v2.md`,
  `docs/reviews/review_2026-08_graph-engine-v2.md` (file:line findings).
- Live evidence: prior 3-node `feature_lifecycle` run; 446 Rust tests + 15 web
  tests green.
- Absent: user research, analytics, benchmarks — future work must not
  fabricate any.

## Product Principles

1. **Local-first.** Never require a network, an account, or a cloud service.
2. **The journal is truth.** Every state change is replayable; nothing that
   matters lives only in memory.
3. **The human sees everything and can steer.** Triage surfaces everything
   waiting on attention; the UI never hides a pending decision.
4. **Evolve, don't fork.** Reuse the existing building blocks — the canvas,
   the endpoints, the reducer — before inventing new ones.
5. **Failures are visible, never silent.** A failed action renders an error;
   cost figures are estimates and say so.

## Accessibility & Inclusion

- One operator on a desktop browser. Keyboard and screen-reader basics per the
  web-UI spec's a11y pass: roles and labels on controls, non-color state cues
  (glyphs alongside color), `aria-live` on transcripts and feeds.
- Dark, dense, professional UI; no reliance on color alone for state.
