// Wire shapes mirroring `supervisor-core` + the daemon API. Rust structs
// serialize snake_case by default; only fields marked `rename_all = "camelCase"`
// differ.

export type WorkspaceState = "off" | "on" | "draining" | "error";
export type AgentState =
  | "unknown" | "spawning" | "working" | "idle"
  | "waiting_input" | "blocked_permission" | "error";
export type NodeState =
  | "pending" | "ready" | "running" | "done" | "failed"
  | "blocked" | "needs_decision";
export type AgentMode = "foreground" | "background";

export interface Workspace {
  id: string;
  path: string;
  port: number | null;
  server_pid: number | null;
  state: WorkspaceState;
  cmux_ws: string | null;
  layout_path: string | null;
  updated_at: string;
}

export interface Agent {
  workspace_id: string;
  agent_id: string;
  role: string;
  model: string | null;
  session_id: string | null;
  driver: "opencode" | "cmux";
  mode: AgentMode;
  state: AgentState;
  confidence: number;
}

export interface DoneWhen {
  ack?: string | null;
  approved?: boolean | null;
  match?: string | null;
}

export interface LoopBack {
  on?: string;
  small: string;
  big: string;
}

export type OnError =
  | string // "delegate" | "skip"
  | { rerun: { max: number } };

export interface NodeDef {
  id: string;
  role: string;
  agent_id?: string | null;
  depends_on: string[];
  start_template: string;
  done_when: DoneWhen;
  on_error: OnError;
  gate?: string | null;
  loop_back?: LoopBack | null;
  mode: AgentMode;
  timeout_secs?: number | null;
}

export interface GraphDef {
  id: string;
  name: string;
  nodes: NodeDef[];
}

export interface GraphRecord {
  id: string;
  name: string;
  data: string;
  version: number;
  active: boolean;
  updated_at: string;
}

export interface NodeStateRow {
  graph_id: string;
  node_id: string;
  state: NodeState;
  attempt: number;
  started_at: string | null;
  finished_at: string | null;
  error: string | null;
}

export interface UsageRow {
  id: string;
  workspace_id: string;
  agent_id: string;
  model: string | null;
  ts: string;
  prompt_tokens: number;
  completion_tokens: number;
  cost_cents: number | null;
}

export interface MetricsTotals {
  messages_delivered: number;
  errors: number;
  decisions: number;
  nodes_done: number;
  nodes_failed: number;
  tokens: number;
  cost_cents: number | null;
}

export interface Metrics {
  since: string;
  totals: MetricsTotals;
  per_workspace: Record<string, Partial<MetricsTotals>>;
  per_agent: Record<string, Partial<MetricsTotals>>;
  time_series: Array<{ ts: string; messages: number; errors: number; cost_cents: number | null }>;
}

export interface TranscriptMessage {
  role: string;
  ts: string;
  text: string;
  // I-29: the driver serializes usage as camelCase (promptTokens /
  // completionTokens) — the wire, not snake_case.
  usage?: { promptTokens: number; completionTokens: number } | null;
}

export interface DecisionRecord {
  id: string;
  signature: string;
  situation: unknown;
  decision: unknown;
  outcome: { result?: string; success?: boolean; note?: string } | null;
  ts: string;
}

export interface Proposal {
  id: string;
  rule_toml: string;
  signature: string;
  cluster_size: number;
  confidence: number;
  status: "pending" | "applied" | "rejected" | "expired";
  created_at: string;
  resolved_at: string | null;
}

// The internal bus event (§4.18). Tagged by `topic`.
export type BusEvent =
  | { topic: "signal"; signal: string; ws: string; agent: string; [k: string]: unknown }
  | { topic: "workflow"; event: string; graph: string; node?: string; [k: string]: unknown }
  | { topic: "fleet"; kind: string; workspace_id?: string; agent_id?: string; [k: string]: unknown }
  | { topic: "decision"; [k: string]: unknown }
  | { topic: "inbox"; kind: string; [k: string]: unknown }
  | { topic: "human"; kind: string; [k: string]: unknown };
