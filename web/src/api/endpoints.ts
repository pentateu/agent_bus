// Thin typed wrappers over the daemon's `/api/v1/*` endpoints (§4.16 + the UI
// additions).

import { get, post, put } from "./client";
import type {
  Agent,
  DecisionRecord,
  GraphDef,
  GraphRecord,
  Metrics,
  NodeStateRow,
  Proposal,
  TranscriptMessage,
  UsageRow,
  Workspace,
} from "./types";

export const api = {
  health: () => get<{ healthy: boolean; workspaces: number }>("/api/v1/health"),

  workspaces: () => get<Workspace[]>("/api/v1/workspaces"),
  workspace: (ws: string) => get<Workspace>(`/api/v1/workspaces/${encodeURIComponent(ws)}`),
  agents: (ws: string) => get<Agent[]>(`/api/v1/workspaces/${encodeURIComponent(ws)}/agents`),

  workspaceOn: (ws: string) => post<{ workspace: string; state: string }>(`/api/v1/workspaces/${encodeURIComponent(ws)}/on`),
  workspaceOff: (ws: string, graceful = true) =>
    post(`/api/v1/workspaces/${encodeURIComponent(ws)}/off`, { graceful }),
  resume: () => post<{ state: string }>("/api/v1/resume"),

  sendMessage: (ws: string, agent: string, body: string, priority = "normal") =>
    post(`/api/v1/workspaces/${encodeURIComponent(ws)}/agents/${encodeURIComponent(agent)}/message`, { body, priority }),
  transcript: (ws: string, agent: string, limit = 50) =>
    get<TranscriptMessage[]>(`/api/v1/workspaces/${encodeURIComponent(ws)}/agents/${encodeURIComponent(agent)}/messages?limit=${limit}`),
  respondPermission: (ws: string, agent: string, pid: string, response: "allow" | "deny", remember = false) =>
    post(`/api/v1/workspaces/${encodeURIComponent(ws)}/agents/${encodeURIComponent(agent)}/permissions/${encodeURIComponent(pid)}`, { response, remember }),
  abortAgent: (ws: string, agent: string) =>
    post(`/api/v1/workspaces/${encodeURIComponent(ws)}/agents/${encodeURIComponent(agent)}/abort`),
  attachAgent: (ws: string, agent: string) =>
    post<{ attach: string; spawned: boolean }>(`/api/v1/workspaces/${encodeURIComponent(ws)}/agents/${encodeURIComponent(agent)}/attach`),

  graphs: () => get<GraphRecord[]>("/api/v1/graphs"),
  graph: (id: string) => get<GraphRecord>(`/api/v1/graphs/${encodeURIComponent(id)}`),
  graphNodes: (ws: string | undefined, id: string) =>
    get<NodeStateRow[]>(
      `/api/v1/graphs/${encodeURIComponent(id)}/nodes${ws ? `?ws=${encodeURIComponent(ws)}` : ""}`,
    ),
  saveGraph: (id: string, data: string) => put(`/api/v1/graphs/${encodeURIComponent(id)}`, { data }),
  startGraph: (ws: string, graph: string, vars: Record<string, string> = {}) =>
    post(`/api/v1/workspaces/${encodeURIComponent(ws)}/graphs/${encodeURIComponent(graph)}/start`, { vars }),

  usage: (params: { ws?: string; agent?: string; since?: string } = {}) => {
    const q = new URLSearchParams();
    if (params.ws) q.set("ws", params.ws);
    if (params.agent) q.set("agent", params.agent);
    if (params.since) q.set("since", params.since);
    return get<{ rows: UsageRow[]; count: number }>(`/api/v1/usage?${q.toString()}`);
  },
  metrics: (since?: string) => {
    const q = since ? `?since=${encodeURIComponent(since)}` : "";
    return get<Metrics>(`/api/v1/metrics${q}`);
  },

  decisions: () => get<DecisionRecord[]>("/api/v1/decision-log"),
  proposals: () => get<Proposal[]>("/api/v1/bakeback/proposals"),
  previewBakeback: () => post<{ created: Proposal[]; pending: Proposal[] }>("/api/v1/bakeback/preview"),
  applyProposal: (id: string) => post(`/api/v1/bakeback/proposals/${encodeURIComponent(id)}/apply`),
  rejectProposal: (id: string) => post(`/api/v1/bakeback/proposals/${encodeURIComponent(id)}/reject`),
};

/** Parse a stored graph JSON into a GraphDef (handles missing fields and
 * malformed data without crashing the page — review minor). */
export function parseGraph(data: string): GraphDef {
  try {
    const value = JSON.parse(data) as Partial<GraphDef>;
    return { id: value.id ?? "", name: value.name ?? "", nodes: value.nodes ?? [] };
  } catch {
    return { id: "", name: "", nodes: [] };
  }
}
