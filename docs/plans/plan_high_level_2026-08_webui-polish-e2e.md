# Supervisor Web UI polish + Playwright e2e (U6) — high-level plan

> **Kind:** high-level plan (product + architecture intent). Not the detailed
> software spec yet.
> **Status:** designed (iterating). Ledger: `docs/ledger.md` →
> `plan_high_level_2026-08_webui-polish-e2e`.
> **Sibling:** [`plan_high_level_2026-08_webui-polish-e2e.md`](plan_high_level_2026-08_webui-polish-e2e.md)
> **Product:** the supervisor web UI in `web/` (Vite + React + TS), served by
> `supervisor-daemon` at `http://127.0.0.1:4198/ui/`.
> **System hub:** [`docs/specs/2026-08-14-supervisor-webui-detailed-design.md`](../specs/2026-08-14-supervisor-webui-detailed-design.md)
> **Detail software design:** `docs/plans/plan_2026-08_webui-polish-e2e.md` (not yet written)
>
> Last updated: 2026-08-14.

## Requirements (locked)

1. Fix the live stream. The SPA must receive SSE events from the daemon.
   Today it does not: `web/src/api/sse.ts:60` fetches `/api/v1/events` without
   the `Authorization` header, and `crates/supervisor-daemon/src/api.rs:99`
   puts every `/api/v1/*` route behind bearer auth. The live dashboard, the
   canvas animations, and the agent dialog all depend on this one fix.
2. Add the two missing pages: intake and rules. Backend endpoints exist
   (`GET /api/v1/intake` at `api.rs:95`, `GET/POST /api/v1/rules` +
   `POST /rules/reload` at `api.rs:85-86`). The SPA has no page for either
   (`web/src/pages/` holds only Agent, Dashboard, Decisions, Graphs).
3. Ship a Playwright e2e suite per the spec's test list (§7 of the web-UI
   spec): dashboard render + live SSE update, live mini-canvas animate +
   green-on-ACK, editor add/wire/edit/save/re-open, agent dialog message echo
   + permission allow/deny. No Playwright harness exists today
   (`web/package.json` has vitest only; no `web/e2e/`, no `playwright.config.*`).
4. Run an accessibility pass: roles/aria on canvas nodes and controls,
   non-color state cues, `aria-live` on the transcript. The spec's U6
   milestone names it; nothing exists today (no `aria-*`, `role`, or
   `data-testid` in `web/src`).
5. Keep verification green: `cargo test --workspace`, `cargo clippy
   --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`,
   `cd web && npm run test && npm run build`. E2E must run headless in CI
   without a live opencode server.

## Current vs target

| Surface | Current | Target |
|---|---|---|
| SSE live feed | 401 — never connects (`sse.ts:60` vs `api.rs:99`) | Bearer header on the fetch-stream; dashboard + canvases animate |
| Pages | dashboard, graphs, decisions, agent dialog | + intake list, + rules list/add/reload |
| E2E | none | Playwright suite in `web/e2e/`, seeded via the API, runs headless |
| a11y | none (`aria-*`, `role`, `data-testid` absent) | roles + labels on controls, non-color state cues, `aria-live` transcript |
| Editor property panel | role, start_template, done_when.ack, mode only (`web/src/pages/Graphs.tsx:89-117`) | full §5.3 panel: agent_id, on_error, gate, loop_back, timeout_secs, done_when approved/match |
| Failure states | bare `—`/empty, react-query errors unhandled | error surface per page; empty-state copy |

## Design

### 1. SSE bearer auth fix

`web/src/api/sse.ts` reads the in-memory token (module-scope, set by
`bootstrapToken`) and passes it as `Authorization: Bearer <token>` on the
fetch-stream request, mirroring `web/src/api/client.ts:35-45`. No backend
change. This is the spine of U6 — the e2e `@live` tests and the polish both
sit on top of it. Do it first.

### 2. Intake + Rules pages

Both pages are thin read/write surfaces over existing endpoints, following the
existing `useQuery`/`useMutation` pattern in `pages/Decisions.tsx`.

- **Intake** (`/intake`): list `GET /api/v1/intake` rows; show kind, title,
  severity, linked `graph_id`, `received_at`; link the graph id to the graph
  page.
- **Rules** (`/rules`): list `GET /api/v1/rules`; TOML textarea add
  (`POST /api/v1/rules`) + reload button (`POST /rules/reload`) per §5.5.
- Nav entries in `web/src/app.tsx`; routes in `parseRoute`.

### 3. Playwright e2e suite

- **Location + toolchain:** `web/e2e/`, `@playwright/test` added to
  `web/package.json` (npm, matching the app — a deliberate deviation from the
  `tester.md` Bun note). Scripts: `test:e2e`, `test:e2e:headed`,
  `test:e2e:report`.
- **Harness** (`web/e2e/run-e2e.sh`): boot a scratch supervisor-daemon on
  4198 with `open_supervisor_workspace=false`, `npm run build:install` to put
  the bundle in `~/.supervisor/ui`, then run Playwright with
  `baseURL=http://127.0.0.1:4198/ui/`. Shut the daemon down after.
- **Auth in tests:** read `~/.supervisor/api-token` and navigate with the token
  in the hash (`page.goto('/ui/#token=' + token)`), exercising the same
  bootstrap the CLI uses (`crates/supervisor-cli/src/main.rs:467` web fn).
- **Seeding:** `beforeAll` drives the API to create a workspace + install a
  graph; `afterAll` cleans up. No live LLM — repeatable and hermetic. The
  `@live` canvas/agent tests need the chain observable; gate them on a fake or
  scrubbed driver so they run headless in CI (see Open questions).
- **Test inventory** (spec §7, verbatim intent):
  - `@smoke` dashboard: workspaces/agents/metrics render; SSE event updates
    state live (this is the test that proves the auth fix).
  - `@live` mini-canvas: node → `running` (spinner), → `done` (green) on ACK.
  - `@smoke` editor: palette add → wire → property edit → save via PUT →
    re-open → persisted.
  - `@smoke` agent dialog: send → transcript echo; permission allow/deny.
  - `@critical` missing-token gate: no token → "run `supervisor web`" screen.
  - New: intake list renders + links; rules add + reload round-trips.
  - Tags per `tester.md` (locators `getByRole`/label before testid; screenshots
    are failure evidence only).
- **Ownership boundary:** tests live in `web/e2e/`; the harness script and
  config are build tooling. Follow `tester.md`'s rule — never touch production
  code to make a test pass; product gaps become findings.

### 4. Accessibility pass

- Canvas nodes (`web/src/components/WorkflowCanvas.tsx` StateCard) get
  `role`/`aria-label`; glyphs already carry state (✓/✕/⛔/!/⚠) so color is not
  the only cue. Decorative spinner gets `aria-hidden`.
- Controls: `aria-label` on icon-only buttons; palette buttons already have
  text labels.
- Agent dialog: transcript becomes `role="log"` `aria-live="polite"`; compose
  input gets a label.
- Focus management for the editor property panel; `alt`/hidden on decorative
  glyphs. Asserted by the e2e `@critical` a11y checks, not just eyeballed.

### 5. Editor property panel completion + failure states

- Extend `web/src/pages/Graphs.tsx` properties panel to the full node field
  set per §5.3 (agent_id, on_error select, gate, loop_back small/big,
  timeout_secs, done_when approved/match), reusing `lib/graph-edit.ts` pure
  helpers.
- A small shared error/empty surface (react-query error state is currently
  unhandled across all pages) so failures render a message, not silence.

## Boundaries

- **In:** the five polish items + the e2e harness + suite, in `web/` only.
- **Out:** backend changes (all endpoints needed already exist; if an e2e test
  surfaces a backend bug, that is a finding to the dev, not a test edit).
- **Out:** the live `opencode serve` chain itself — `supervisor smoke` already
  owns that proof (`crates/supervisor-cli/src/main.rs:271`); the e2e suite
  observes state, it does not drive real LLM turns.
- **Out:** the `@live` tests that require a real agent turn are either faked or
  deferred (Open questions 2).

## Open questions

1. npm vs Bun for `web/e2e/`. Recommendation: npm, single lockfile with the
   app; update `tester.md`'s Bun note as a separate doc change.
2. Do the `@live` canvas/agent-dialog e2e tests boot a fake/scrubbed driver
   (hermetic, CI-safe) or a real `opencode serve` (slow, needs the chain
   green)? Recommendation: fake for CI; a tagged `@live` real-chain variant
   runs only where `supervisor smoke` is green.
3. Should `aria-label`/`data-testid` be added during the a11y pass, or only
   where a test needs a stable locator? Recommendation: prefer roles/labels,
   add `data-testid` only for canvas-internal nodes.

## Implementation sketch (after lock)

1. SSE bearer fix (`sse.ts`) — spine; verify live in dev.
2. Playwright harness + config + `run-e2e.sh` + API seeding (foundation for
   every e2e task).
3. `@smoke` static tests: dashboard render, missing-token gate, graphs list.
4. Intake + Rules pages (polish surfaces double as e2e targets).
5. Accessibility pass + a11y assertions in the suite.
6. `@live` canvas + agent-dialog tests (gated on the chain / fake driver).
7. Editor property panel completion + failure-state surface.

## Related

- Spec: [`docs/specs/2026-08-14-supervisor-webui-detailed-design.md`](../specs/2026-08-14-supervisor-webui-detailed-design.md) (§7 e2e list, §8 U6 milestone, §5 pages).
- Tester contract: [`docs/agents/tester.md`](../agents/tester.md).
- Verification bar: [`docs/agents/dev-orchestrator.md`](../agents/dev-orchestrator.md).
