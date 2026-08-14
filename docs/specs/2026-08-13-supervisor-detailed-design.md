# agent-bus Orchestration v2 — Fleet Supervisor: Detailed Design

**Status:** Draft (build blueprint — component-by-component, implementable)
**Date:** 2026-08-13
**Revision:** reviewed + revised against an external agent's 15-point review
  (all 15 accepted and fixed — see §13; the review's build-blocking claims
  about `opencode serve --dir/--agent` and `cmux new-surface --command` were
  re-verified live before being applied). A second review round raised 9 minor
  findings — all accepted and fixed (manager/supervisor-agent separation in
  the example roster, role→agent resolution + missing-role policy, human-gate
  ACK fields, small/big loop_back targets, counter-store rebuild source,
  docs node dependencies, CLI redundancy, role enum, adopt-or-kill health
  check).
**Depends on:** `2026-08-13-agent-bus-orchestration-v2` (approved high-level design)
**Audience:** coding agents, reviewers

This document is the level below the approved high-level design. Every
component, interface, data shape, and API is specified concretely so a coding
agent can implement without asking questions. Where the high-level plan said
"the supervisor does X", this document says how, with what types, tables, and
calls.

External facts were verified against the live system on 2026-08-13:
- opencode server API (v1 surface on `opencode-ai@beta`): `prompt_async` →
  204, `/session/status` returns `idle|busy|retry` but **omits idle sessions**
  (idle is SSE-only), `/event` SSE streams `session.status`, `session.idle`,
  `session.diff`, `message.*`, `server.heartbeat`.
- cmux CLI contract (`manafow-ai/cmux` `docs/cli-contract.md`): the exact
  command surface in §7.

---

## 1. Scope and architecture summary

One long-lived **supervisor** process (Rust, tokio) owns:

- the **fleet**: every managed project = one opencode `serve` on a fixed
  loopback port + a cmux workspace of panes, one pane per agent session;
- the **workspace lifecycle**: `on`, `off` (graceful), `resume`;
- the **workflow engine**: declarative DAGs (default dev lifecycle, bug flow)
  advanced offline on events;
- the **decision layer**: offline rules → LLM fallback → bake-back;
- the **API**: loopback HTTP (phase-2 webUI/DAG-editor ready);
- the **CLI + dashboard** (ratatui).

agent-bus (the existing bus) is **kept as-is** and is the bridge to agents on
other harnesses (Claude Code, Pi, Codex, …). The supervisor does NOT replace
it; it talks to opencode agents directly over the opencode API and to cmux over
its CLI, and uses agent-bus only for cross-harness messages that opencode cannot
reach.

### 1.1 Component list

| ID | Component | Kind | Key outward interface |
|----|-----------|------|----------------------|
| C1 | supervisor core | async runtime, wiring | tokio tasks, event bus |
| C2 | fleet store | persistent state | SQLite + journal |
| C3 | port allocator | pure logic | alloc/free/reserve |
| C4 | workspace manager | async service | cmux + opencode clients |
| C5 | cmux client | external client | `cmux` CLI |
| C6 | opencode client | external client | opencode HTTP API |
| C7 | SSE observer | external client | opencode `/event` |
| C8 | queue & delivery | async service | per-agent inboxes |
| C9 | rule engine | pure logic | rules (TOML + code) |
| C10 | workflow engine | pure logic | graph data |
| C11 | LLM fallback | external client | opencode manager session |
| C12 | decision log + bake-back | async service | rules file |
| C13 | supervisor agent | opencode session | slash commands → CLI |
| C14 | CLI + dashboard | binary | `supervisor` command |
| C15 | HTTP API | axum server | loopback REST + SSE |

---

## 2. Tech stack and crate layout

### 2.1 Stack

| Concern | Choice | Rationale |
|---|---|---|
| Language | Rust, edition 2024 | workspace contract; `#![forbid(unsafe_code)]` |
| Async runtime | tokio (full features) | long-lived binary only |
| HTTP client (opencode) | `reqwest` (rustls, json, stream) | SSE + JSON |
| SSE parse | `eventsource-stream` | line-delimited `data:` frames |
| SQLite | `rusqlite` (bundled) | single file, no server |
| Config | `serde` + `toml` | `supervisor.toml`, rules |
| CLI | `clap` (derive) | `supervisor` subcommands |
| HTTP API | `axum` | phase-2 REST + SSE |
| TUI | `ratatui` | dashboard |
| Process spawn | `tokio::process` | opencode/agent launch |
| Sockets (cmux, agent-bus) | `tokio::net::UnixStream` + `interprocess` | local IPC |
| Errors | `thiserror` in core, `anyhow` at binary boundary | workspace contract |
| Logging | `tracing` + `tracing-subscriber` | structured logs |
| Time | `chrono`/`time` | timestamps |

### 2.2 Crates

```
crates/
  supervisor-core/     # pure: types, port math, state machine, rules, DAG, event model. No I/O, no async.
  supervisor-daemon/   # the long-lived supervisor process (all async services + clients).
  supervisor-cli/      # `supervisor` command: status/on/off/resume/log/rules/dag + dashboard + api server.
```

`supervisor-core` must stay pure and unit-testable, matching the existing
`agent-bus-core` contract. Everything that touches a socket, process, or file
lives in `supervisor-daemon`.

### 2.3 Module map (supervisor-daemon)

```
src/
  main.rs
  config.rs            # load supervisor.toml, fleet.json
  state.rs             # in-memory FleetState (mirror of SQLite), mutation via journal
  db.rs                # SQLite schema + access
  journal.rs           # append-only JSONL writer + replay
  bus.rs               # internal event bus (tokio broadcast) + topic enum
  clients/
    mod.rs
    opencode.rs        # C6
    cmux.rs            # C5
    sse.rs             # C7
    manager.rs         # C11
  services/
    workspace.rs       # C4
    inbox.rs           # C8
    rules.rs           # C9
    workflow.rs        # C10
    bakeback.rs        # C12
  api.rs               # C15 axum
  cli.rs               # C14 (invoked by supervisor-cli over the same lib)
  dashboard.rs         # ratatui
```

---

## 3. Data model

### 3.1 SQLite schema (`~/.supervisor/supervisor.db`)

```sql
PRAGMA journal_mode = WAL;

CREATE TABLE workspace (
  id          TEXT PRIMARY KEY,          -- slug: "iot_platform"
  path        TEXT NOT NULL,             -- absolute project dir
  port        INTEGER,                   -- opencode server port, null when off
  state       TEXT NOT NULL DEFAULT 'off', -- off|on|draining|error
  cmux_ws     TEXT,                      -- cmux workspace handle/name
  layout_path TEXT,                      -- supervisor.toml path (project-local)
  updated_at  TEXT NOT NULL
);

CREATE TABLE agent (
  workspace_id TEXT NOT NULL REFERENCES workspace(id) ON DELETE CASCADE,
  agent_id     TEXT NOT NULL,            -- "dev_01"
  role         TEXT NOT NULL,            -- dev|reviewer|tester|designer|memory-keeper|supervisor (manager is a supervisor-server session, not a per-project role)
  model        TEXT,                     -- provider/model
  session_id   TEXT,                     -- opencode session id
  state        TEXT NOT NULL DEFAULT 'unknown', -- unknown|spawning|working|idle|waiting_input|blocked_permission|error
  confidence   REAL NOT NULL DEFAULT 1.0, -- 1.0 = observed
  PRIMARY KEY (workspace_id, agent_id)
);

CREATE TABLE port (
  port         INTEGER PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspace(id),
  allocated_at TEXT NOT NULL
);

CREATE TABLE inbox_entry (
  id           TEXT PRIMARY KEY,         -- ulid
  workspace_id TEXT NOT NULL,
  agent_id     TEXT NOT NULL,
  priority     TEXT NOT NULL DEFAULT 'normal', -- normal|high
  body         TEXT NOT NULL,
  from         TEXT,                     -- "human" | "workflow" | agent id
  kind         TEXT NOT NULL DEFAULT 'instruction',
  in_reply_to  TEXT,
  ack_for      TEXT,                     -- task id this acks
  delivered    INTEGER NOT NULL DEFAULT 0,
  delivered_at TEXT,
  created_at   TEXT NOT NULL
);
CREATE INDEX inbox_undelivered ON inbox_entry(workspace_id, agent_id, delivered, priority, created_at);

CREATE TABLE graph (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  data         TEXT NOT NULL,            -- JSON: nodes + edges
  version      INTEGER NOT NULL DEFAULT 1,
  active       INTEGER NOT NULL DEFAULT 1,
  updated_at   TEXT NOT NULL
);

CREATE TABLE node_state (
  graph_id     TEXT NOT NULL REFERENCES graph(id),
  node_id      TEXT NOT NULL,
  state        TEXT NOT NULL DEFAULT 'pending', -- pending|ready|running|blocked|done|failed|needs_decision
  attempt      INTEGER NOT NULL DEFAULT 0,
  started_at   TEXT,
  finished_at  TEXT,
  error        TEXT,
  PRIMARY KEY (graph_id, node_id)
);

CREATE TABLE decision (
  id          TEXT PRIMARY KEY,
  signature   TEXT NOT NULL,
  situation   TEXT NOT NULL,             -- JSON snapshot
  decision    TEXT NOT NULL,             -- JSON action
  outcome     TEXT,                      -- JSON result, filled later
  ts          TEXT NOT NULL
);
CREATE INDEX decision_sig ON decision(signature);

CREATE TABLE rule (
  id         TEXT PRIMARY KEY,
  toml       TEXT NOT NULL,              -- full [[rule]] block
  source     TEXT NOT NULL,              -- data|code|bakeback
  confidence REAL NOT NULL,
  approved   INTEGER NOT NULL DEFAULT 0,
  active     INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE TABLE proposal (
  id          TEXT PRIMARY KEY,          -- proposal_<ulid> (stable across restarts)
  rule_toml   TEXT NOT NULL,
  signature   TEXT NOT NULL,
  cluster_size INTEGER NOT NULL,
  confidence  REAL NOT NULL,
  status      TEXT NOT NULL DEFAULT 'pending', -- pending|applied|rejected|expired
  created_at  TEXT NOT NULL,
  resolved_at TEXT
);

CREATE TABLE intake (
  id          TEXT PRIMARY KEY,
  source      TEXT NOT NULL,             -- github|app-feedback|cli
  kind        TEXT NOT NULL,             -- bug|feature|feedback
  title       TEXT NOT NULL,
  body        TEXT NOT NULL,
  severity    TEXT,
  refs        TEXT NOT NULL DEFAULT '[]',-- JSON array
  graph_id    TEXT,
  received_at TEXT NOT NULL
);

CREATE TABLE journal (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  type       TEXT NOT NULL,
  data       TEXT NOT NULL,              -- JSON payload
  ts         TEXT NOT NULL
);
```

### 3.2 Journal protocol

- File: `~/.supervisor/journal.jsonl`, append-only, `fsync` on write.
- Every mutating event is journaled BEFORE the in-memory state / DB is updated:
  `workspace.state`, `agent.state`, `inbox.enqueue`, `inbox.deliver`,
  `workflow.transition`, `decision.record`, `rule.merge`, `port.alloc`,
  `port.free`.
- **The journal is the source of truth.** On start, replay the journal to
  rebuild the in-memory state; the SQLite DB is a **rebuildable projection**
  (index/query layer) that is re-derived from the journal on start or
  corruption. Both live under `~/.supervisor/`.
- Journal records are idempotent (they carry the full new state value), so
  replay is safe and a truncated/corrupt tail only loses trailing events.
- **Reconciliation rule (single authority):** the journal wins. If the DB and
  journal disagree after replay, the DB is dropped and rebuilt from the
  journal. The DB is never written without a matching journal entry first.

### 3.3 Fleet state (in-memory + `fleet.json`)

`fleet.json` is a human-readable snapshot at `~/.supervisor/fleet.json`,
rewritten atomically after each mutation (cache for the supervisor agent /
dashboard / humans). The journal is authoritative; `fleet.json` and the DB
are both caches/projections.

---

## 4. Component specifications

### 4.1 C2 Fleet store

- **DB access** via `rusqlite` in a dedicated tokio task (all DB calls go
  through an `mpsc` to a single writer thread, or use `r2d2`+`tokio-rusqlite`
  pool). Simpler choice: a single owned `Connection` behind a `tokio::sync::Mutex`.
- **API** (Rust):
  - `fn upsert_workspace(ws: &Workspace)`
  - `fn get_workspace(id) -> Option<Workspace>`
  - `fn list_workspaces() -> Vec<Workspace>`
  - `fn upsert_agent(a: &Agent)` / `fn list_agents(ws_id)`
  - `fn enqueue_inbox(e: &InboxEntry)` / `fn claim_inbox(ws, agent, limit)`
  - `fn upsert_graph(g: &Graph)` / `fn get_graph(id)`
  - `fn set_node_state(...)` / `fn get_node_state(graph, node)`
  - `fn append_decision(d: &Decision)` / `fn decisions_since(ts)`
  - `fn upsert_rule(r: &Rule)` / `fn active_rules() -> Vec<Rule>`
- **Journaling**: all mutations are also `journal.append(type, data)`.

### 4.2 C3 Port allocator

- Pure logic in `supervisor-core`:
  ```rust
  pub struct PortAllocator { range: RangeInclusive<u16>, reserved: BTreeSet<u16>, used: BTreeSet<u16> }
  impl PortAllocator {
      pub fn new(range, reserved: impl IntoIterator<Item = u16>) -> Self;
      pub fn reserve(&mut self, port: u16) -> Result<(), PortError>; // already used?
      pub fn alloc(&mut self) -> Option<u16>;   // lowest free in range, never a reserved port
      pub fn free(&mut self, port: u16);
  }
  ```
- **Reserved set**: ports the supervisor itself binds — the API port (4198)
  and the supervisor-workspace port (4199) — are excluded from allocation.
  Configured via `[supervisor] reserved_ports` in `supervisor.toml`; `alloc`
  and `reserve` refuse them.
- Persisted via `port` table. On `supervisor on`, before using a reserved port,
  do a bind probe: `TcpListener::bind(("127.0.0.1", port)).await` — if it
  succeeds, the port is free; **drop the listener immediately** before the
  `serve` child binds it (holding it longer would make `serve`'s own bind
  fail). If the probe fails, the port is externally occupied → log and pick
  another (except for a recorded workspace — see §4.3 adopt-or-kill, which
  never switches ports).

### 4.3 C4 Workspace manager

State machine for a workspace: `off → on → draining → off`.

```
off ──on──> on ──off(graceful)──> draining ──(all idle, timeout)──> off
  │                               │
  └──────on (no-op if on)          └──error → logged, stays draining, retry
```

**`on(workspace_id)`** — idempotent. Steps:

1. If state == `on`, no-op.
2. Load layout `supervisor.toml` from the project (see §6.1).
3. **Adopt-or-kill the recorded port** (see §4.3 "resume"): if `fleet.json`
   records a port for this workspace and the process there passes the
   PID-match + `/global/health` check, adopt it; if something else holds it
   (an orphan from a prior crash), kill it and continue with the same port so
   session ids stay valid. Then `PortAllocator.alloc()` for first-time
   workspaces; record in `port` table.
4. **cmux**: `cmux new-workspace --name <ws> --cwd <path>`; record handle.
5. **Server**: spawn `opencode serve --port <port> --hostname 127.0.0.1` as a
   child process with **`.current_dir(project)`** (opencode resolves the
   project by CWD walk-up; `serve` has **no** `--dir`/`--agent` flags —
   verified). Record PID. Wait for `/global/health` 200 (retry up to ~30s).
6. **Sessions**: for each agent, if `session_id` recorded and still valid
   (`GET /session/{id}` → 200) reuse it; else `POST /session` (title:
   `<ws>/<agent>`, and the agent's role/model from the layout) and record the
   new id. **The agent/role/model for a session is set here, per-session, via
   `POST /session` — never via a `serve` flag.** Log truncation if a session
   id is gone (per high-level plan).
7. **Panes — foreground vs background.** Every agent has a **mode**
   (`foreground` | `background`) in `supervisor.toml`:
   - **Foreground** (default): create a cmux terminal surface per agent and
     send the attach command into it:
     1. `cmux new-surface --type terminal --workspace <ws>
        --working-directory <project>` → surface handle (cmux `new-surface`
        takes no `--command`; verified — the shell starts in the working dir).
     2. `cmux send --workspace <ws> --surface <s>
        "opencode attach http://127.0.0.1:<port> --session <session_id>"` then
        send `\r` (Enter) to run it. One visible pane per agent, so the human
        watches each session and can type into it. This is the pane model
        chosen in review: attach-per-agent to the project's single `serve`.
     - Alternative (note): cmux `new-surface --type agent-session
       --provider opencode --working-directory <path>` launches a **native**
       opencode agent session in its own pane — a different model (per-agent
       opencode, not attach-to-shared-serve). Not the default; recorded here
       for a future project that prefers per-agent native surfaces.
   - **Background**: no pane is created. The agent runs headless on the
     server; the supervisor drives it through the driver exactly like a
     foreground agent (same rules, same workflows). A background agent can be
     attached to later with `supervisor attach <ws> <agent>` (spawns a cmux
     terminal surface, then `cmux send` the `opencode attach --session` line)
     — but that is optional and creates no persistent pane.
   - **Subagents** (spawned by the Task tool / session forks) never get
     panes — normal opencode behavior.
8. Set workspace state `on`; journal.
9. Start the SSE observer for this workspace (C7).
10. Drain any undelivered inbox entries for this workspace's agents (C8).

**`off(workspace_id, graceful=true)`** — supervisor owns teardown end to end:

1. Set state `draining`; journal. Stop delivering new inbox items to agents
   (mark as paused but keep queued).
2. For each agent, if state is `busy`/`working`: wait up to `graceful_timeout`
   (default 120s) for idle (driver `status`, or the SSE `SessionIdle` signal).
   Graceful means let the turn finish; only `abort` on timeout.
3. After all idle (or timeout): `kill` the opencode child process (SIGTERM,
   then SIGKILL after 10s). Release the port in the allocator.
4. **Close the panels**: `cmux close-surface` for every foreground pane (and
   `cmux close-workspace` for the workspace if no other surfaces remain). The
   supervisor keeps `session_id`s, ports, and the layout record in fleet state,
   so there is nothing for cmux to restore — **resume is supervisor-owned and
   rebuilds the workspace from scratch.**
5. Set state `off`; journal.

**`resume()`** (all previously-`on` workspaces, serial, for logs):

1. For each workspace in fleet state with `state != off` OR flagged
   `resume: true`: call `on(workspace_id)`. `on` re-creates the cmux
   workspace, panes, and server from the fleet record — panels are rebuilt
   from scratch, sessions resume by id.
2. **Adopt-or-kill the recorded port** (resume correctness): before re-spawning
   `serve`, check the recorded port. A surviving process is adopted **only if
   both** its PID matches the recorded PID **and** `GET /global/health` on the
   recorded port answers 200 with our version — the health check guards against
   PID reuse by an unrelated process (a recycled PID would otherwise be
   mistaken for our live server). If the port is held by anything else (an
   orphan from a prior crash, or another process), kill it and respawn on the
   **same** port. Session ids are tied to their server's port, so picking a
   different port would break resume; we never fall back to another port for a
   recorded workspace.
3. Validate each agent `session_id`; stale → new session + truncation log.
4. Re-subscribe SSE.

### 4.4 C5 cmux client

All calls shell out to the `cmux` binary (path from config, default
`/Applications/cmux.app/Contents/Resources/bin/cmux`, or `cmux` on PATH).
`CMUX_SOCKET_PATH`/`CMUX_SOCKET_PASSWORD` respected. Prefer `--json` where the
contract supports it; parse stdout.

Rust trait:

```rust
#[async_trait]
pub trait CmuxClient: Send + Sync {
    async fn ping(&self) -> Result<()>;
    async fn capabilities(&self) -> Result<serde_json::Value>;
    async fn list_workspaces(&self) -> Result<Vec<CmuxWorkspace>>; // cmux list-workspaces
    async fn new_workspace(&self, name: &str, cwd: &Path) -> Result<CmuxHandle>;
    async fn new_surface(&self, ws: &CmuxHandle, working_dir: &Path)
        -> Result<CmuxHandle>;   // cmux new-surface --type terminal --working-directory
    async fn send_cmd(&self, ws: &CmuxHandle, surface: &CmuxHandle, text: &str)
        -> Result<()>;           // cmux send <text> then \r (Enter) — used to run
                                 // "opencode attach --session <id>" in a foreground pane
    async fn focus_pane(&self, ws: &CmuxHandle, pane: &CmuxHandle) -> Result<()>;  // cmux focus-pane
    async fn select_workspace(&self, ws: &CmuxHandle) -> Result<()>;              // cmux select-workspace
    async fn close_surface(&self, ws: &CmuxHandle, surface: &CmuxHandle) -> Result<()>;
    async fn close_workspace(&self, ws: &CmuxHandle) -> Result<()>;
    async fn read_screen(&self, ws: &CmuxHandle, surface: &CmuxHandle) -> Result<String>; // cmux read-screen
    async fn send(&self, ws: &CmuxHandle, surface: &CmuxHandle, text: &str) -> Result<()>; // cmux send
    async fn send_key(&self, ws: &CmuxHandle, surface: &CmuxHandle, key: &str) -> Result<()>;
    async fn notify(&self, ws: &CmuxHandle, title: &str, body: &str) -> Result<()>;
}
```

Handles: use the stable UUID forms. All commands accept `--workspace`,
`--surface`/`--pane` handles (UUID or ref like `workspace:2`). The client caches
handles in the workspace record.

cmux event subscription (for C7-adjacent lifecycle signals) uses
`cmux events --reconnect --cursor-file ~/.supervisor/cmux-events.seq` streamed
as newline JSON; frames carry `seq`/`id` for dedupe. Supervisor consumes:
`surface.*`, `workspace.*`, `notification.*`, and `feed.item.*` events to enrich
signals (e.g. a human focusing a pane, notifications).

### 4.5 C6 opencode client

Base URL per workspace: `http://127.0.0.1:<port>`. Auth: `OPENCODE_SERVER_PASSWORD`
(via basic auth, user `opencode`). The supervisor passes the password to each
`opencode serve` process via env.

```rust
pub struct OpencodeClient { base: Url, client: reqwest::Client }

impl OpencodeClient {
    pub async fn health(&self) -> Result<bool>;                       // GET /global/health
    pub async fn create_session(&self, title: &str) -> Result<SessionId>;          // POST /session
    pub async fn get_session(&self, id) -> Result<Option<Session>>;                // GET /session/{id}
    pub async fn session_status(&self) -> Result<HashMap<SessionId, SessionStatus>>; // GET /session/status (map; idle omitted)
    pub async fn prompt_async(&self, id, parts: Vec<Part>, agent: Option<&str>,
                              format: Option<OutputFormat>) -> Result<()>;
                              // POST /session/{id}/prompt_async; format = { type:"json_schema", schema } (optional; model-dependent)
    pub async fn messages(&self, id, limit) -> Result<Vec<Message>>;               // GET /session/{id}/message?limit=
    pub async fn todo(&self, id) -> Result<Vec<Todo>>;                             // GET /session/{id}/todo
    pub async fn respond_permission(&self, id, permission_id, response) -> Result<()>; // POST /session/{id}/permissions/{pid}
    pub async fn abort(&self, id) -> Result<()>;                                    // POST /session/{id}/abort
    pub async fn revert(&self, id) -> Result<()>;                                   // POST /session/{id}/revert
    pub async fn summarize(&self, id) -> Result<()>;                                // POST /session/{id}/summarize
}
```

`Part` = opencode message part (`{ type: "text", text }` etc.). `prompt_async`
returns 204; the server serializes prompts per session, giving the
turn-boundary contract.

### 4.6 C7 SSE observer

- One task per **on** workspace. Streams `GET /event` on the workspace's
  opencode server.
- Reconnect: on EOF/error, exponential backoff (1s,2s,4s,… max 60s). `server.heartbeat`
  frames reset a watchdog; if no heartbeat for 90s, force reconnect.
- Each frame → parse → map to a `Signal` → publish on the internal bus.

`Signal` enum (in core):

```rust
pub enum Signal {
    SessionStatus { ws: String, agent: String, status: SessionStatus }, // busy|retry|idle
    SessionIdle { ws: String, agent: String },
    StepStarted { ws: String, agent: String },
    StepEnded { ws: String, agent: String },
    StepFailed { ws: String, agent: String, error: Option<String> },
    ToolFailed { ws: String, agent: String, name: String },
    PermissionAsked { ws: String, agent: String, permission_id: String },
    NeedsInput { ws: String, agent: String },
    SessionError { ws: String, agent: String },
    Diff { ws: String, agent: String },
    Heartbeat { ws: String },
}
```

Mapping from opencode event `type` + properties (verified inventory):

| opencode event | Signal | sessionID in payload? |
|---|---|---|
| `session.status` status=`busy` | SessionStatus(busy) | yes (`properties.sessionID`) |
| `session.status` status=`retry` | SessionStatus(retry) | yes |
| `session.status` status=`idle` | SessionIdle | yes |
| `session.idle` | SessionIdle | yes |
| `session.next.step.started` | StepStarted | yes (`data.sessionID`) |
| `session.next.step.ended` | StepEnded | yes |
| `session.next.step.failed` | StepFailed | yes |
| `session.next.tool.failed` | ToolFailed | yes |
| `session.error` | SessionError | yes |
| `message.part.updated` w/ `permission` shape | PermissionAsked | yes (`properties.sessionID`) |
| `permission.asked` | PermissionAsked | yes |
| `session.diff` | Diff | yes (`properties.sessionID`) |
| `server.heartbeat` | Heartbeat | no — **unscoped**, ignored for agent mapping |

The agent is derived from `sessionID` via the workspace→agent table (each
agent has its own session id). If unknown, ignore. `server.heartbeat` is used
only for connection liveness, never for agent state. (All the per-session
frames above carry their `sessionID`; only `server.connected` /
`server.heartbeat` are connection-scoped.)

### 4.7 Driver abstraction (opencode driver now, cmux driver next)

Every agent is driven through a **driver**, so the supervisor is not married to
the opencode API. Today we implement the **opencode driver**; the **cmux
driver** comes later, for harnesses that expose no API (Claude Code, Pi,
Codex, Grok, …) — those agents get driven by typing into their terminal pane
(`cmux send`) and observed by reading the pane (`cmux read-screen`).

```rust
#[async_trait]
pub trait AgentDriver: Send + Sync {
    fn kind(&self) -> DriverKind;                       // Opencode | Cmux
    async fn send(&self, a: &AgentRef, msg: &str, format: Option<OutputFormat>)
        -> Result<SendReceipt>;
    async fn read_last_output(&self, a: &AgentRef, limit: usize) -> Result<String>;
    async fn read_structured(&self, a: &AgentRef) -> Result<Option<serde_json::Value>>;
    async fn status(&self, a: &AgentRef) -> Result<AgentState>;
    async fn abort(&self, a: &AgentRef) -> Result<()>;
}

pub enum DriverKind { Opencode, Cmux }
```

- **OpencodeDriver**: `send` → `prompt_async` (may include `format` for
  structured output); `read_structured` → the last assistant message's
  structured field; `status` → `session.status` + SSE signals; `abort` →
  `/session/{id}/abort`.
- **CmuxDriver (future)**: `send` → `cmux send --surface <pane>`; `read_last_output`
  → `cmux read-screen`; `status` → pane heuristic + cmux `surface.*` events;
  `read_structured` → always `None` (no structured output over a terminal).
- The driver is chosen per agent from `agent.driver` in `supervisor.toml`
  (default `opencode`). A workspace may mix drivers (e.g. dev on opencode,
  a Claude Code reviewer on cmux) — each agent's pane is whatever harness it
  runs, and the supervisor treats them identically through the trait.

### 4.8 C8 queue & delivery

- One inbox per `(workspace, agent)`, ordered by `(priority DESC, created_at)`.
- Enqueue: journal + DB (`inbox_entry`). Deliver:
  - When the agent transitions to **idle** (driver `status` == idle, or the
    SSE `SessionIdle` signal for the opencode driver), and the workspace is
    `on`, claim the next undelivered entry and `driver.send(...)`. Mark
    delivered on success (204 for opencode).
  - If the workspace is `off`/`draining`, entries stay queued.
- High priority is pulled ahead of normal within the same inbox.
- Delivery is **at-least-once**; completion is the workflow's concern (the
  ACK mechanism below), not the transport's.

### 4.9 Completion & acknowledgment — the ACK mechanism (zoomed in)

**Problem.** A workflow node starts an agent on a task. How does the
supervisor learn the task finished? opencode sessions have no bus `ack`
field, and a session going `idle` only means the *turn* ended, not that the
*task* succeeded.

**Verified constraints (live, beta 2026-08-11):**
- `prompt_async` **accepts** `format: { type: "json_schema", schema }` at the
  top level (with `parts`) → 204. Structured output is requestable.
- **But the provider/model may reject it**: `deepseek-v4-flash` in thinking
  mode fails with `APIError: Thinking mode does not support this tool_choice`.
  So structured output is **model-dependent** — it must be a fast path, never
  the only path.
- `session.idle` / `session.status: idle` arrive only on SSE. Idle ≠ task done.

**The contract (driver-agnostic).** Every node's `start_template` instructs the
agent that its **final message must be a single JSON object**:

```
When you finish, your final message must be EXACTLY:
{"task_id":"<task_id>","status":"done|failed|blocked","summary":"<one line>"}

Human-gate nodes add two OPTIONAL fields, present only when status is "done":
{"task_id":"...","status":"done","summary":"...",
 "approved":true|false, "needs_revision":"none|small|big"}
  approved:false + needs_revision:small  → loop back to this node's gate (human loop)
  approved:false + needs_revision:big    → loop back to the pre-review node (re-run agent review)
```

The supervisor resolves completion on turn end in this order:

1. **Structured output** (if we sent `format` and the driver can read it):
   the validated JSON is the completion. This is the preferred fast path for
   models that support it.
2. **Parse the final text as JSON**: if the last output is a single JSON
   object with `task_id`/`status` → completion. This covers models that
   rejected `format` (thinking mode) but still comply with the contract.
3. **Regex ACK line** (universal fallback, incl. cmux driver): scan the last
   output for `(?m)^ACK\s+(\S+)\s+(done|failed|blocked)(?:\s+(.*))?$`.
   (For a human-gate node the regex can also match
   `ACK <task_id> done approved=<true|false> revision=<none|small|big>`.)
4. **`done_when.match`** pattern over the last output (test banners etc.).
5. **No signal** → node stays `running`; a per-node timeout moves it to
   `needs_decision` (manager/human decides: done / rerun / skip / split).

The resolved completion becomes `workflow.ack { task_id, status, summary,
approved?, needs_revision? }` on the bus; the DAG engine matches it against the
running node's `done_when.ack`. Status `failed|blocked` → node `failed` /
`needs_decision`. A human-gate node completes only when `done_when.approved`
is satisfied (`approved:true`); an `approved:false` ACK triggers the node's
`loop_back` target (`needs_revision:small|big` selects the target per the
graph, see §4.11).

**Why layered.** Step 1 alone is unreliable (provider rejections); step 2+3
alone are fragile (LLM formatting drift). Layering gives a strict preference
order while guaranteeing a decision at every turn end — and step 4 + timeout
cover the tail so no node hangs silently.

**Human-in-the-loop nodes** (design review): completion is the human's
approval. Flow: designer agent calls `submit_plan` (Plannotator) → human
annotates in browser → approval/feedback returns to the agent's session →
agent's next message carries the outcome, which the same resolver picks up as
a `done` ACK with the feedback in `summary`. A design node whose ack says
`done` with `approved:true` advances; feedback → the designer agent gets the
revision message (loop).

### 4.10 C9 rule engine

Pure, in `supervisor-core`. Rules in two forms:

**Data rules** (TOML, `rules.toml` / DB `rule` table):

```toml
[[rule]]
id = "rerun_crashed_tester_once"
when = { agent.type = "tester", state = "error", reason = "exit",
         times_errored_in_1h = { lte = 1 } }
confidence = 0.9
action = { kind = "post", to = "$agent",
           body = "Your last run crashed (context below). Re-run once.\n{last_output}" }
```

Supported `when` fields (flat key = path): `agent.role`, `state`, `state.confidence`,
`reason`, counters (`times_errored_in_1h`), `node_id`, `node.state`, `signal`.
Operators: `=`, `!=`, `in`, `lte`, `gte`, `contains`. Unknown keys → rule
never matches (logged on load).

**Counters source (resolved):** counter fields like `times_errored_in_1h`
read from a dedicated **event-counter store** — a small in-memory
`HashMap<(AgentKey, EventKind), VecDeque<Instant>>` (per `Situation`) that the
SSE observer and driver feed as signals arrive, pruned on a rolling window.
It is exposed to the rule evaluator as read-only lookups
(`count(agent, "error", window)`) so rules never mutate it. The counter store
lives in core and is **not journaled** (signals are not journaled, per §4.18),
so it cannot be rebuilt from signal history. Instead it is **rebuilt on start
from journaled `agent.state` transitions**: the journal records every
`agent.state` change (including `→ error`), and the counter store replays those
journaled transitions to repopulate counts such as `times_errored_in_1h`.
Counts that depend on non-journaled signals (e.g. `tool.failed`) start at zero
after a restart and fill in as signals arrive — rule confidence for those is
therefore first-run conservative, which is acceptable.

**Code rules**: registered Rust `fn(situation) -> Option<Decision>`.

```rust
pub struct Situation {
    pub ws: String,
    pub agent: AgentId,
    pub agent_role: String,
    pub state: AgentState,
    pub signals: Vec<Signal>,
    pub node: Option<NodeRef>,
    pub inbox_depth: usize,
    pub last_output: Option<String>,
}
pub struct Decision { pub action: Action, pub confidence: f64 }

pub enum Action {
    Post { to: AgentId, body: String },
    RespondPermission { permission_id: String, allow: bool },
    Transition { to: AgentState },
    StartWorkflow { graph: String, params: Map },
    FocusPane { ws: String, agent: AgentId },
    Noop,
    Escalate { reason: String },
}
```

Cascade: score all matching data rules and code rules → highest confidence ≥
`threshold` (default 0.8) wins; data beats code on ties; none → `Escalate`.
Escalation → C11 (manager). Every executed decision is logged (C12).

Hot reload: `supervisor rules reload` re-reads `rules.toml` + DB `rule` table.

### 4.11 C10 workflow engine

Graph data (JSON, in `graph` table / files under `~/.supervisor/graphs/`). The
**default `feature_lifecycle` graph** below is the authoritative rendering of
the approved plan's §6.1 (feature → prod) and ships verbatim:

```json
{
  "id": "feature_lifecycle",
  "name": "Default feature → prod",
  "nodes": [
    { "id": "brainstorm", "role": "designer", "depends_on": [],
      "start_template": "Research and brainstorm {feature} per {spec}. Finish by returning the ACK JSON contract with task_id=brainstorm.",
      "done_when": { "ack": "brainstorm" }, "on_error": "delegate" },
    { "id": "high_level_design", "role": "designer", "depends_on": ["brainstorm"],
      "start_template": "Write the high-level design for {feature}. Finish by returning the ACK JSON contract with task_id=high_level_design.",
      "done_when": { "ack": "high_level_design" }, "on_error": "delegate" },
    { "id": "hl_agent_review", "role": "reviewer", "depends_on": ["high_level_design"],
      "start_template": "Review the high-level design: verify every claim and completeness; ask 'is there a better solution?' per option. Finish with the ACK JSON contract, task_id=hl_agent_review.",
      "done_when": { "ack": "hl_agent_review" }, "on_error": "delegate" },
    { "id": "hl_human_gate", "role": "designer", "depends_on": ["hl_agent_review"],
      "gate": "plannotator",
      "start_template": "Submit the high-level design via submit_plan for human review. When the human approves or sends feedback, finish with the ACK JSON contract, task_id=hl_human_gate, status reflecting approval; include feedback in summary; set needs_revision=none|small|big per the feedback.",
      "done_when": { "ack": "hl_human_gate", "approved": true }, "on_error": "delegate",
      "loop_back": { "on": "needs_revision",
                     "small": "hl_human_gate",
                     "big": "high_level_design" } },
    { "id": "detailed_design", "role": "designer", "depends_on": ["hl_human_gate"],
      "start_template": "Write the detailed design for {feature} from the approved high-level design. Finish with the ACK JSON contract, task_id=detailed_design.",
      "done_when": { "ack": "detailed_design" }, "on_error": "delegate" },
    { "id": "dd_agent_review", "role": "reviewer", "depends_on": ["detailed_design"],
      "start_template": "Review the detailed design in detail; request human input only if a real decision is needed. Finish with the ACK JSON contract, task_id=dd_agent_review.",
      "done_when": { "ack": "dd_agent_review" }, "on_error": "delegate" },
    { "id": "dev", "role": "dev", "depends_on": ["dd_agent_review"],
      "start_template": "Implement {feature} from the approved detailed design; go through code review cycles, meet standards, unit+integration tests. Finish with the ACK JSON contract, task_id=dev.",
      "done_when": { "ack": "dev" }, "on_error": { "rerun": { "max": 2 } } },
    { "id": "tester_prep", "role": "tester", "depends_on": ["dd_agent_review"],
      "start_template": "Prepare UI automation setup and scripts for {feature} in parallel with dev. Finish with the ACK JSON contract, task_id=tester_prep.",
      "done_when": { "ack": "tester_prep" }, "on_error": "delegate" },
    { "id": "ui_e2e", "role": "tester", "depends_on": ["dev", "tester_prep"],
      "start_template": "Run end-to-end UI tests on {feature} (web + mobile, human-like). Finish with the ACK JSON contract, task_id=ui_e2e.",
      "done_when": { "ack": "ui_e2e" }, "on_error": "delegate" },
    { "id": "docs", "role": "memory-keeper", "depends_on": ["dev", "ui_e2e"],
      "start_template": "Update docs for {feature}. Finish with the ACK JSON contract, task_id=docs.",
      "done_when": { "ack": "docs" }, "on_error": "delegate" },
    { "id": "deploy_dev", "role": "dev", "depends_on": ["ui_e2e", "docs"], "mode": "background",
      "start_template": "Deploy {feature} to the dev env per this project's deploy rules. Finish with the ACK JSON contract, task_id=deploy_dev.",
      "done_when": { "ack": "deploy_dev" }, "on_error": "delegate" },
    { "id": "verify_dev", "role": "tester", "depends_on": ["deploy_dev"],
      "start_template": "Verify {feature} in the dev env. Finish with the ACK JSON contract, task_id=verify_dev.",
      "done_when": { "ack": "verify_dev" }, "on_error": "delegate" },
    { "id": "promote_prod", "role": "dev", "depends_on": ["verify_dev"], "mode": "background",
      "start_template": "Promote {feature} to prod per this project's deploy rules. Finish with the ACK JSON contract, task_id=promote_prod.",
      "done_when": { "ack": "promote_prod" }, "on_error": "delegate" }
  ]
}
```

- **ACK contract:** every node's `start_template` requires the final message
  to be the ACK JSON object (see §4.9). `done_when.ack` matches the
  `task_id`. The deploy/promote nodes run **background** dev agents (no pane);
  per-project deploy rules live in the project (its dev agent knows what
  "deploy to dev" / "promote to prod" means for that repo), and the supervisor
  learns completion the same way — via the ACK contract.
- **Human gates** (`gate: "plannotator"`): a node whose `done_when` includes
  `approved: true` only completes on an ACK whose JSON carries
  `"approved": true` (the agent submits via `submit_plan`, the human approves
  in the browser, the outcome flows back into the agent's next message). An
  `approved:false` ACK with `needs_revision` triggers the node's `loop_back`,
  which maps `needs_revision` to a target per the graph:
  - `small` / `medium` → back to the **gate node itself** (the revised design
    goes straight back to the human — the loop stays with the human);
  - `big` → back to an **earlier agent-review node** (a fresh agent review
    cycle runs before the human sees it again).
  This matches the approved plan's review-cycle rule exactly: small/medium
  changes stay in the human loop; big changes re-run agent review first.
- Node states: `pending → ready → running → done`; `running → failed | blocked |
  needs_decision`. Readiness = all `depends_on` are `done`. On ack/state event the
  engine recomputes readiness for the graph and enqueues start messages (C8) for
  newly ready nodes whose owning agent is idle.
- **Role → agent resolution (resolved):** a node's `role` selects the agent to
  deliver to. Resolution order: (1) the node's explicit `agent_id` if set; else
  (2) the **least-loaded idle agent** with a matching `role` in the workspace
  (fewest queued inbox items, then earliest `session_id`); else (3) the first
  matching-role agent (queue the message, deliver when idle). If **no agent in
  the workspace has the node's role** (e.g. a graph requires `memory-keeper`
  but the roster has none), the node holds at `ready`/`blocked` with a logged
  `missing_role` note and the dashboard surfaces it — the node resumes only when
  a matching-role agent exists (`supervisor add-agent` or config change). This
  is the explicit missing-role policy.
- `on_error`: `delegate` (→ C11), `rerun {max}`, `skip`. Rerun bounds enforced
  in code. Workflow completion = all terminal nodes `done`. `needs_decision`
  nodes pause and post to the manager; the manager's structured decision moves
  the node (`done`, `rerun`, `skip`, `split`).

The default graphs (feature lifecycle, bug flow) ship in
`~/.supervisor/graphs/*.json` and are installable via `supervisor dag apply`.

### 4.12 C11 LLM fallback (manager)

**Distinct role from the supervisor agent (C13).** The manager is the
LLM *decision engine* for the escalation cascade — a **background** opencode
session on the supervisor server (port 4199), using a **`manager` agent
config** (`manager.md` prompt). It is driven entirely programmatically: the
supervisor posts escalations and reads structured decisions. It has **no
pane, no human TUI, and no slash commands**. The human never talks to it
directly; they talk to the supervisor agent (C13) or the dashboard.

- Supervisor drives it with `prompt_async` + **structured output**
  (`format: { type: "json_schema", schema: {...} }` on the prompt) so the
  reply is validated JSON the orchestrator executes directly.
  - **Same caveat as the ACK contract (§4.9):** structured output can be
    rejected by the model/provider (verified: thinking-mode models reject the
    structured-output `tool_choice`). So the manager's decision is resolved
    the same layered way: structured output → parse final JSON → regex → fall
    back to asking again with a stricter instruction. The decision schema is
    small, so `retryCount` and one re-ask usually suffice; on repeated failure
    the escalation surfaces to the dashboard for the human.
- Escalation record sent to the manager (compact):
  ```json
  {
    "escalation_id": "esc_...",
    "situation": "agent=dev_01 ws=iot_platform state=error signals=[step.failed]",
    "candidates": ["rerun", "skip", "delegate"],
    "refs": [
      {"kind":"messages","url":"http://127.0.0.1:4101/session/{id}/message?limit=20"},
      {"kind":"todo","url":"..."}
    ],
    "node": {"graph":"bug_flow","node":"fix","state":"failed"}
  }
  ```
- Decision schema (returned by manager):
  ```json
  {
    "action": "rerun | skip | split | done | post",
    "to": "agent-id?",
    "body": "instruction if post",
    "reason": "short justification",
    "confidence": 0.0
  }
  ```
  **`confidence` semantics:** the manager's own confidence in its decision,
  0.0–1.0. Below 0.5 the supervisor treats the escalation as unresolved and
  surfaces it to the dashboard for the human, rather than acting on a weak
  call. It is recorded in the decision log and feeds bake-back's proposed-rule
  confidence.
- On structured error (manager failed to produce valid output), the escalation
  stays pending and is surfaced on the dashboard for the human.
- Window guard: the manager session's messages are never dumped; only the
  compact escalation + refs. Subagents (Task tool / session fork) do depth.

### 4.13 C12 decision log + bake-back

- `decision` table + `~/.supervisor/decision.jsonl`. Fields: id, signature,
  situation (JSON), decision (JSON), outcome (later), ts.
- **Bake-back** (`supervisor bake-back --preview`): cluster `decision` rows by
  `signature` (normalized: strip ids, keep state+signal+role+node). Signatures
  with ≥ N occurrences (default 3) → generate a proposed `[[rule]]` TOML block
  with confidence = observed outcome success rate (min 0.6). Print as preview.
- **Proposal lifecycle (stable ids, resolved):** every previewed proposal gets
  a stable `proposal_<ulid>` and is persisted in a dedicated `proposal` table
  (id, rule_toml, signature, cluster_size, confidence, status
  `pending|applied|rejected|expired`, created_at). It survives restarts
  between preview and decision. `supervisor bake-back --apply <id>` marks it
  `applied` and inserts the rule into the `rule` table + `rules.toml` →
  `supervisor rules reload`; `--reject <id>` marks `rejected`. Proposals not
  acted on within 30 days expire. `--apply` on an already-applied or rejected
  id is a no-op with a message.
- Gate knob: `bakeback.auto_approve` in `supervisor.toml`
  (`never` (default) | `low_risk` where confidence ≥ 0.9 and signature matches ≥ 10).

### 4.14 C13 supervisor agent

**Distinct role from the manager (C11).** The supervisor agent is the
**human-facing** opencode session in the supervisor workspace (port 4199), a
**foreground** TUI attached to the supervisor server, using a **`supervisor`
agent config** (`supervisor.md` prompt). The human opens it to talk to the
supervisor, ask `/start-workspace`, and read status. It drives the supervisor
binary via the CLI; it is **not** the escalation decision-maker (that is the
manager, C11). The two are separate sessions with separate agent configs on
the same supervisor server.

- Slash commands (implemented as opencode commands calling `supervisor` CLI):
  - `/start-workspace <name>` → `supervisor on <name>`
  - `/status` → `supervisor status`
  - `/off <name>` → `supervisor off <name>`
  - `/rules list|reload` → `supervisor rules ...`
  - `/dag status [graph]` → `supervisor dag status ...`
  - `/log [tail]` → `supervisor log`
- The agent reads `fleet.json` for context and drives the binary via CLI.

### 4.15 C14 CLI

```
supervisor daemon                        headless runtime (launchd target; no TUI)
supervisor status                       all workspaces + agent states + queue depth
supervisor on <project>                 idempotent bring-up
supervisor off <project> [--force]      graceful off (--force skips wait)
supervisor resume                       restore all on-marked workspaces (serial)
supervisor log [--tail N] [--json]      decision log / recent events
supervisor rules list|reload
supervisor bake-back --preview          cluster decision log → proposed rules
supervisor bake-back --apply <id>|--reject <id>   act on a persisted proposal
supervisor dag list|apply <file>|status [id]
supervisor api                          start loopback HTTP API (default 4198)
supervisor dashboard                    ratatui TUI
supervisor add <path>                   register a project + generate supervisor.toml
supervisor attach <ws> <agent>          open a pane attached to a background agent's session
supervisor agents --background          list foreground + background agents / attach status
supervisor ingest <source> <payload>    post an ingested item into the bug/feature intake
```

Exit codes: 0 success, 1 usage, 2 target not found, 3 daemon unreachable.

### 4.16 C15 HTTP API (axum, phase-2-ready)

Loopback only, port 4198 (configurable). Auth: bearer token in
`~/.supervisor/api-token` (generated on first run) or basic auth.

```
GET  /api/v1/health
GET  /api/v1/workspaces
GET  /api/v1/workspaces/{id}
POST /api/v1/workspaces/{id}/on
POST /api/v1/workspaces/{id}/off          { "graceful": true }
POST /api/v1/resume
GET  /api/v1/workspaces/{id}/agents       (mode: foreground|background, state)
POST /api/v1/workspaces/{id}/agents/{aid}/message   { "body": "...", "priority": "normal" }
POST /api/v1/workspaces/{id}/agents/{aid}/attach    (spawn a pane on a background agent)
GET  /api/v1/graphs            GET/PUT/DELETE /api/v1/graphs/{id}
GET  /api/v1/graphs/{id}/nodes
GET  /api/v1/rules             POST /api/v1/rules
GET  /api/v1/decision-log?since=
GET  /api/v1/bakeback/proposals            (previewed proposals; status filter)
POST /api/v1/bakeback/proposals/{id}/apply
POST /api/v1/bakeback/proposals/{id}/reject
GET  /api/v1/events            (SSE stream of internal bus events)
POST /api/v1/ingest            { "source": "github|app-feedback|cli", "payload": {...} }
GET  /api/v1/intake            (recent intake items; status filter)
```

Phase-2 webUI/mobile/DAG editor are thin clients over this.

### 4.17 Ingestion layer (bug / feature intake)

The supervisor has an **ingestion layer** — the answer to "where do bug
reports and feature requests enter?" It is implemented case by case, each
source is a small adapter that normalizes an incoming item into the intake
model and posts it to the bug/feature channel.

- **Intake model** (per item): `{ id, source, kind: bug|feature|feedback,
  title, body, severity?, refs[], received_at }`. Stored in a new
  `intake` table; each item that starts a workflow links to its `graph_id`.
- **Sources (phase 1):**
  - **GitHub issues** — the public bug/issue inbox for a project. An adapter
    polls the repo's issues API (or a webhook) and posts new issues as `bug`
    intake items → bug workflow.
  - **In-app feedback** — products with their own feedback flow (screenshot +
    recent logs + message) POST to `POST /api/v1/ingest`; the product's code
    wires its own mechanism to that endpoint. Feedback that looks like a bug
    → bug workflow; otherwise a feature/feedback item for triage.
  - **CLI** — `supervisor ingest <source> <payload>` for scripts and the
    supervisor agent.
- Each source adapter is just "receive → normalize → post to intake → rule
  engine decides which workflow (bug flow, feature flow) to start". No new
  transport; everything rides the internal event bus.

### 4.18 Internal event bus

- `tokio::sync::broadcast` with topics as a tagged enum:
  ```rust
  pub enum BusEvent {
      Signal(Signal),
      Workflow(WorkflowEvent),   // node.ready, node.done, node.failed, ack
      Inbox(InboxEvent),         // enqueued, delivered
      Fleet(FleetEvent),         // workspace.on, workspace.off, agent.state
      Decision(DecisionRecord),
      Human(HumanEvent),         // CLI / API / slash input
  }
  ```
- Journaled topics: `Workflow`, `Inbox.enqueue/deliver`, `Fleet`, `Decision`.
  Cheap `Signal`s are not journaled.

---

## 5. Startup, shutdown, launchd

- **launchd user agent** (`~/Library/LaunchAgents/com.agentbus.supervisor.plist`):
  `ProgramArguments = ["<supervisor-bin>", "daemon"]`, `RunAtLoad=true`,
  `KeepAlive={ SuccessfulExit = false }`, `StandardOut/ErrPath` to
  `~/.supervisor/logs/`.
- **`supervisor daemon` is the headless runtime** (the launchd target; no TUI,
  runs the loopback API + all services). `supervisor` with no subcommand
  prints help; `supervisor dashboard` attaches to the running daemon via the
  loopback API; `supervisor status` queries the API. `daemon` is a first-class
  subcommand in the §4.15 command surface.
- **Project discovery.** On start, the supervisor scans every immediate child
  of the workspace root (`~/development/*`) and auto-registers any folder
  containing a `supervisor.toml`. `supervisor add <path>` registers a new
  project and generates its `supervisor.toml` from an interactive prompt
  (name, roster, roles, models, port). `supervisor remove <name>` unregisters
  (state kept unless `--purge`).
- Daemon start sequence:
  1. Load config; open DB; replay journal.
  2. Discover/register projects (scan + recorded fleet).
  3. Bind loopback API (4198).
  4. Ensure supervisor workspace: cmux `new-workspace` if absent, then spawn
     `opencode serve --port 4199` with **`.current_dir(~/development)`** (no
     `--dir`/`--agent` — `serve` has neither; verified). The supervisor agent's
     session gets a TUI attached to the supervisor server (the human can open
     it to interact with the supervisor).
  5. Start the SSE observer for every workspace marked `on`.
  6. Start workflow engine, inbox drainers, rule engine, ingestion adapters.
  7. Log "supervisor ready".
- Graceful shutdown (SIGTERM/SIGINT): drain inboxes, flush journal, close
  children, exit 0. launchd restarts on unexpected exit.

---

## 6. Config file formats

### 6.1 `supervisor.toml` (project-local, in each project root)

```toml
[project]
name = "iot_platform"        # workspace slug
path = "~/development/iot_platform"

[server]
port = 4101                  # fixed or "auto" (allocator)
default_agent = "dev"        # default agent/role for sessions created via POST /session
                             # (NOT a serve flag — serve has no --agent; verified)

[[agent]]
id = "dev_01"
role = "dev"
model = "anthropic/claude-sonnet-4"
driver = "opencode"          # opencode (default) | cmux
mode = "foreground"          # foreground (pane, default) | background (headless)

[[agent]]
id = "reviewer_01"
role = "reviewer"
model = "anthropic/claude-haiku-4"
mode = "background"          # no pane; attach later with supervisor attach

[[agent]]
id = "reviewer_cmux"
role = "reviewer"
model = "anthropic/claude-haiku-4"
driver = "cmux"               # example: a Claude Code / Codex / Pi reviewer driven via its cmux pane
mode = "foreground"

# The manager (C11) is NOT a per-project roster agent. It is one background
# opencode session on the supervisor server (port 4199), shared across the
# fleet, driven programmatically. Do not add a manager entry here.

[workflow]
graphs = ["feature_lifecycle", "bug_flow"]   # install these graphs for this project

[ingest]
github = { repo = "acme/iot_platform", poll_secs = 300 }   # optional GitHub issues adapter
```

### 6.2 `supervisor.toml` (supervisor root `~/.supervisor/supervisor.toml`)

```toml
[supervisor]
workspace_root = "~/development"
port_range = [4100, 4299]
reserved_ports = [4198, 4199]            # API + supervisor workspace; never allocated to a project
api_port = 4198
supervisor_workspace_port = 4199
open_workspaces_on_start = true        # resume all previously-on workspaces
cmux_bin = "/Applications/cmux.app/Contents/Resources/bin/cmux"
opencode_bin = "opencode"              # on PATH (or absolute)

[workflow]
default_graphs = ["feature_lifecycle", "bug_flow"]

[rule]
threshold = 0.8
reload = "auto"

[bakeback]
min_occurrences = 3
auto_approve = "never"                 # never | low_risk

[graceful]
off_timeout_secs = 120

[ingest]
sources = ["github", "app-feedback", "cli"]   # enabled intake adapters
```

### 6.3 `rules.toml` (global + per-project overrides)

Global: `~/.supervisor/rules.toml`. Per-project:
`~/development/<proj>/.supervisor/rules.toml` merged (project wins).

---

## 7. External API contracts (verified)

### 7.1 opencode (v1 surface, works on v1.18+ and v2 beta)

| Endpoint | Method | Body | Response |
|---|---|---|---|
| `/global/health` | GET | — | `{healthy, version}` |
| `/session` | POST | `{title?, parentID?, agent?, model?}` | Session |
| `/session/{id}` | GET | — | Session |
| `/session/status` | GET | — | `{ [sessionId]: {type: idle\|busy\|retry, ...} }` (idle sessions OMITTED) |
| `/session/{id}/prompt_async` | POST | `{parts:[{type:"text",text}], agent?, model?, format?}` | 204; `format` = `{type:"json_schema", schema}` for structured output |
| `/session/{id}/message?limit=` | GET | — | `[{info, parts}]` |
| `/session/{id}/todo` | GET | — | `Todo[]` |
| `/session/{id}/permissions/{pid}` | POST | `{response: allow\|deny, remember?}` | bool |
| `/session/{id}/abort` | POST | — | bool |
| `/session/{id}/revert` | POST | — | bool |
| `/session/{id}/summarize` | POST | `{providerID, modelID}` | bool |
| `/event` | GET (SSE) | — | `server.connected`, `session.*`, `message.*`, `permission.*`, `server.heartbeat` |

Verified: `prompt_async` queues serially per session (turn-boundary contract);
idle signals arrive only on SSE; one server → many clients; second server on
same port → ServeError. `format` is **accepted** on `prompt_async` but is
**model-dependent**: verified that `deepseek-v4-flash` in thinking mode
rejects the structured-output `tool_choice` with `APIError: Thinking mode does
not support this tool_choice`. The ACK/manager contracts therefore resolve
completion with the layered fallbacks in §4.9, never structured-only.

### 7.2 cmux (CLI contract)

The supervisor uses these (all accept `--workspace <handle>`, `--json`):

| Purpose | Command |
|---|---|
| connectivity | `cmux ping` |
| list | `cmux list-workspaces` / `cmux tree --all` |
| create workspace | `cmux new-workspace --name <n> --cwd <path> [--env ...]` |
| create agent pane | `cmux new-surface --type terminal --workspace <ws> --working-directory <path>` then `cmux send "opencode attach ..."` (new-surface takes no `--command`; verified). Alternative native pane: `cmux new-surface --type agent-session --provider opencode --working-directory <path>` |
| focus | `cmux focus-pane --pane <p> --workspace <ws>` / `cmux select-workspace --workspace <ws>` |
| read output | `cmux read-screen --workspace <ws> --surface <s> [--lines N]` |
| send input | `cmux send --workspace <ws> --surface <s> <text>` / `cmux send-key ...` |
| close | `cmux close-surface --surface <s>` / `cmux close-workspace --workspace <ws>` |
| notify | `cmux notify --title ... --body ... --workspace <ws>` |
| events | `cmux events --reconnect --cursor-file <f>` (newline JSON, `seq`/`id`) |

Handles: UUID or refs (`workspace:2`, `pane:3`). Socket: `CMUX_SOCKET_PATH`
(default `~/.local/state/cmux/cmux-501.sock`), password via
`--password`/`CMUX_SOCKET_PASSWORD`. Events retained 4096, frames ≤ 16 KiB.

### 7.3 agent-bus (bridge, unchanged)

agent-bus stays as the **cross-harness bridge**: the one mechanism that reaches
agents which are neither opencode-API nor cmux-driven (or anything outside the
supervisor's own plane). Existing CLI: `post`, `wait`, `read`, `history`,
`follow`. No changes to agent-bus required.

**Bridge relay loop (the missing mechanism, now specified).** The supervisor
runs one long-lived **bridge worker** per registered partition:

- **Outbound (supervisor → external agent):** a rule/action targeting an agent
  whose driver is neither `opencode` nor `cmux` (or explicitly routed to the
  bus) is delivered via `agent-bus post <partition>/<agent>` — the same
  topic-addressed channel the external harness already listens on
  (`wait`/hook). The bridge worker is just the sender-side adapter.
- **Inbound (external agent → supervisor):** for each partition, a dedicated
  worker loops on `agent-bus wait <partition>/supervisor --as supervisor
  --timeout <long>` (blocking, zero tokens while idle). Each delivered message
  is normalized into a `Signal`/`InboxEntry` and published on the internal
  event bus exactly like an opencode message. This is what closes the loop for
  harnesses that post acknowledgements or requests to the bus.
- **Monitoring:** `agent-bus history <partition>/** --since <window>` (non-
  consuming) is polled on a timer so the supervisor's dashboard shows
  cross-harness traffic without stealing messages.
- The bridge worker is idempotent: `wait` uses the cursor, so a crash/restart
  of the supervisor resumes where it left off (same contract as any
  agent-bus subscriber).

This makes the "bridge" concrete: it is a pair of adapter tasks (send-side
`post`, receive-side `wait`) per partition, fronting the internal event bus —
no new transport, no agent-bus changes.

---

## 8. State machine (agent)

| State | Provenance | Entered by |
|---|---|---|
| unknown | observed | default |
| spawning | observed | workspace on |
| working | observed | step.started / status busy |
| idle | observed | session.idle / step.ended |
| waiting_input | observed | needs_input signal |
| blocked_permission | observed | permission.asked |
| error | observed/inferred | step.failed / session.error / tool.failed ×N |
| recovery | observed | new turn starts (→ working) |

Transitions validated by a table in core; illegal transitions are rejected and
logged. Every transition journaled + published.

---

## 9. Security

- Loopback only (phase 1): bind `127.0.0.1`; API token file mode 0600.
- Each `opencode serve` gets `OPENCODE_SERVER_PASSWORD` from the supervisor
  secret store (`~/.supervisor/secrets.json`, 0600); the supervisor uses basic
  auth with it. Same for cmux socket password if set.
- No secrets in logs or journal. envsitter-guard in every project config
  protects `.env` from agents.
- Supervisor agent session never receives project secrets except via env to
  its own server.

---

## 10. Error handling & reliability

- **At-least-once**: inbox delivery journaled before the POST; `delivered`
  flag set only after 204. On crash, replay re-queues undelivered. Idempotency
  of a retried prompt is the agent's job (task ids in ACK lines).
- **No silent failures**: every error surfaces to the dashboard + `supervisor log`.
- **Port conflicts**: bind probe; log + retry with next free port.
- **opencode server crash**: child process exit → workspace state `error`;
  rule engine may rerun or delegate; `supervisor on` is idempotent recovery.
- **SSE loss**: reconnect with backoff; `GET /session/status` fallback for
  busy/retry; idle is assumed only for known sessions after a fresh
  status poll + heartbeat window (see decision in high-level plan).
- **Journal corruption**: a bad line is skipped with a warning and counted;
  the journal remains the source of truth and the DB projection is rebuilt
  from the surviving journal. A corrupt journal never silently changes state —
  it is surfaced to the dashboard and `supervisor log`.
- **Authority (resolved):** journal = source of truth; SQLite and `fleet.json`
  = projections, rebuilt from the journal on start (§3.2). No second
  source of truth exists.

---

## 11. Testing strategy

**Core (pure, unit):** port allocator (alloc/free/reserve/collision,
reserved-ports never handed out), state
machine transition table, rule matching (match/no-match/conflict/below
threshold), rule counters (rolling-window count lookups), DAG readiness from
partial node sets, rerun bounds, human-gate (`approved`/`needs_revision` +
`loop_back`) semantics, bake-back
clustering + proposal lifecycle (preview → apply/reject/expire), journal
idempotent replay, situation signature normalization, **ACK resolver**
(structured JSON → parsed JSON → regex → match → no-signal, with the strict
precedence order).

**Daemon (integration, real SQLite + fake cmux + real opencode serve):**
- `on` idempotence; `off` graceful (busy → waits for idle → drains → off →
  panels closed, session_ids kept); supervisor-owned resume rebuilds from scratch;
- **resume adopt-or-kill**: a recorded port held by an orphaned serve is killed
  and respawned on the same port; a live PID is adopted; a recorded workspace
  never switches ports;
- foreground vs background agents: background has no pane, is driven the same,
  attach spawns a pane later;
- bug-from-off: intake item → workspace spun up → bug flow runs → ACK → done;
- **driver abstraction**: same workflow runs against an opencode agent and a
  fake cmux driver (type into pane, read screen, regex ACK);
- prompt delivery ordering (high first, FIFO within priority), queued-while-busy;
- ACK layering: structured output path (model supports it), JSON-text path
  (thinking-mode model), regex fallback, `done_when.match`, timeout →
  `needs_decision`;
- decision cascade: confident rule acts; uncovered → escalation → manager
  structured decision executed + logged (low-confidence decisions surface to
  the dashboard instead of acting); bake-back → proposal → apply → next time offline;
- restart replay: workspace table + inboxes + node states restored (journal is
  the authority; DB rebuilt from it);
- cross-project port collision: two workspaces with same agent ids get distinct ports;
- SSE reconnect + status fallback; sessionID-less frames (heartbeat) never map to an agent;
- ingestion: github-issue adapter → intake → bug workflow; `/api/v1/ingest`
  round-trip;
- agent-bus bridge: outbound `post` + inbound `wait` relay worker delivers a
  cross-harness message into the internal bus (fake agent-bus harness).

**cmux client (integration, real cmux):** ping, create workspace + surface,
focus, read-screen, send, close — against the running app.

**API (integration):** loopback endpoints round-trip; SSE stream emits bus
events; `attach` on a background agent creates a pane bound to its session.

---

## 12. Implementation order (suggested milestones)

1. **M1 — core**: `supervisor-core` types, port allocator, state machine, rule
   engine, DAG engine, ACK resolver, journal model. 100% unit-tested.
2. **M2 — store + bus**: SQLite schema, journal replay, internal event bus.
3. **M3 — clients**: opencode client (opencode driver) + cmux client +
   the `AgentDriver` trait + integration tests.
4. **M4 — workspace manager**: on/off/resume (foreground + background agents,
   supervisor-owned teardown + rebuild) against real cmux + real opencode.
5. **M5 — delivery + workflows**: inboxes, prompt delivery, default graphs,
   ACK contract wiring end to end.
6. **M6 — decision layer**: rule wiring, manager escalation with the layered
   fallback, decision log, bake-back.
7. **M7 — CLI + API + dashboard**: full command surface (incl. `add`,
   `attach`, `agents`, `ingest`), axum API, ratatui.
8. **M8 — ingestion + launchd + supervisor agent**: github/app-feedback/CLI
   adapters, auto-start, slash commands, end-to-end polish.
9. **M9 — cmux driver (future)**: drive Claude Code / Pi / Codex / Grok via
   their terminal panes; no supervisor-core changes (driver trait only).

---

## 13. Decisions (resolved 2026-08-13 during detailed-design review)

1. **Panes = attach-per-agent** (foreground default): one cmux surface per
   agent running `opencode attach --session <id>` to the project's single
   `serve`. The supervisor agent has its own TUI attached to the supervisor
   server. Subagents and background agents have no terminal.
2. **Driver abstraction** — opencode driver now, cmux driver later. The cmux
   driver drives non-API harnesses (Claude Code, Pi, Codex, Grok) via pane
   `send`/`read-screen`; the trait makes it invisible to the workflow engine.
3. **Ingestion layer** — bug/feature intake is adapter-based: GitHub issues
   first (public bug inbox), in-app feedback POSTs to `/api/v1/ingest`
   (screenshot + logs), CLI for scripts. Each adapter normalizes → intake →
   rule engine picks the workflow.
4. **ACK is layered** (structured JSON → parse-final-JSON → regex ACK →
   `done_when.match` → timeout/needs_decision) because structured output is
   model-dependent (verified rejection on thinking-mode models). This is the
   completion contract for both workflow nodes and the manager.
5. **Store** — SQLite + journal, kept minimal memory footprint (single DB
   connection behind a mutex, WAL, no extra services).
6. **Project discovery** — auto-scan `~/development/*` for `supervisor.toml`;
   `supervisor add <path>` registers + generates the file.
7. **Resume is supervisor-owned** — `off` closes panels and tears down; resume
   rebuilds the workspace from scratch from the fleet record. Nothing left for
   cmux to restore.
8. **Deploy = background dev agent** — per-project deploy rules; the supervisor
   learns completion via the ACK contract; the agent can be attached later.
9. **Foreground + background agents** — both driven identically; `supervisor
   agents --background` and `supervisor attach` for the headless ones.
10. **State path moved (deviation recorded):** the approved plan put fleet
    state at `~/development/.supervisor/`; the detailed design uses
    `~/.supervisor/` (one supervisor-owned state dir, independent of any
    project subtree, per the "supervisor is the whole system" decision).
    Recorded here so the deviation is explicit.
11. **Store authority (resolved):** the journal is the source of truth; SQLite
    and `fleet.json` are rebuildable projections (§3.2, §10). Not two masters.
12. **Reserved ports:** 4198 (API) and 4199 (supervisor workspace) are
    excluded from the allocator's range (§4.2, §6.2 `reserved_ports`).
13. **Resume adopt-or-kill:** recorded ports are adopted (our PID) or killed
    (orphan) but never changed, so session ids stay valid across crashes (§4.3).
14. **Manager vs supervisor agent are distinct** sessions/roles (C11 vs C13):
    manager = background decision engine; supervisor agent = human-facing TUI.
15. One-server-per-project kept; SSE-only idle accepted (mitigated); graceful-off
    safe boundary = end of current turn; bake-back gate `never` initially.

---

## 14. References

- Approved high-level plan: `2026-08-13-agent-bus-orchestration-v2`
  (plannotator-approved).
- `docs/specs/2026-08-10-orchestration.md` (superseded by this, kept for
  history).
- `docs/specs/2026-08-06-agent-bus-design.md` (bus, unchanged).
- cmux CLI contract: `manafow-ai/cmux` `docs/cli-contract.md`.
- opencode API: `/doc` OpenAPI on any `opencode serve`; SDK `@opencode-ai/sdk`.
