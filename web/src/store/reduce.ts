// The pure live-state reducer (§6.4): `BusEvent[]` → `LiveState`. Unit-tested;
// drives the dashboard and every WorkflowCanvas. No network, no React.

import type { AgentState, BusEvent, NodeState, WorkspaceState } from "../api/types";

export interface LiveState {
  workspaceStates: Record<string, WorkspaceState>;
  agentStates: Record<string, Record<string, AgentState>>;
  /** ws → graph → node → state. */
  nodeStates: Record<string, Record<string, Record<string, NodeState>>>;
  /** ws → agent → pending permission id (or null). */
  permissionPending: Record<string, Record<string, string | null>>;
  lastEvents: BusEvent[];
}

export function initialLiveState(): LiveState {
  return {
    workspaceStates: {},
    agentStates: {},
    nodeStates: {},
    permissionPending: {},
    lastEvents: [],
  };
}

const MAX_EVENTS = 200;

const workflowNode = (e: BusEvent): { ws?: string; graph: string; node: string; state: NodeState } | null => {
  if (e.topic !== "workflow") return null;
  const ev = e.event as string;
  const node = e.node as string;
  if (!node) return null;
  switch (ev) {
    case "node_ready":
      return { graph: e.graph, node, state: "ready" };
    case "node_started":
      return { graph: e.graph, node, state: "running" };
    case "node_done":
      return { graph: e.graph, node, state: "done" };
    case "node_failed":
      return { graph: e.graph, node, state: "failed" };
    case "node_blocked":
      return { graph: e.graph, node, state: "blocked" };
    case "node_needs_decision":
      return { graph: e.graph, node, state: "needs_decision" };
    default:
      return null;
  }
};

export function reduce(prev: LiveState, event: BusEvent): LiveState {
  const next: LiveState = {
    ...prev,
    workspaceStates: { ...prev.workspaceStates },
    agentStates: prev.agentStates,
    nodeStates: prev.nodeStates,
    permissionPending: prev.permissionPending,
    lastEvents: [...prev.lastEvents, event].slice(-MAX_EVENTS),
  };

  if (event.topic === "fleet") {
    const kind = event.kind as string;
    if (kind === "workspace_state" || kind === "workspaceState") {
      const ws = event.workspace as { id: string; state: WorkspaceState };
      if (ws?.id) next.workspaceStates[ws.id] = ws.state;
    } else if (kind === "agent_state" || kind === "agentState") {
      const wid = event.workspace_id as string;
      const aid = event.agent_id as string;
      const st = event.state as AgentState;
      if (wid && aid && st) {
        const perWs = next.agentStates[wid] ?? {};
        next.agentStates = { ...next.agentStates, [wid]: { ...perWs, [aid]: st } };
      }
    }
  } else if (event.topic === "workflow") {
    const update = workflowNode(event);
    if (update) {
      const wid = update.ws ?? "";
      // The bus event lacks a workspace; graph→workspace is ambiguous without
      // context. We key by graph only in a per-workspace map when the payload
      // carries it; fall back to the graph key under a synthetic "?" ws key
      // that the pages overlay with their own wiring. See note in reduce tests.
      const perWs = next.nodeStates[wid] ?? {};
      const perGraph = perWs[update.graph] ?? {};
      next.nodeStates = {
        ...next.nodeStates,
        [wid]: { ...perWs, [update.graph]: { ...perGraph, [update.node]: update.state } },
      };
    }
  } else if (event.topic === "signal") {
    const wid = event.ws as string;
    const aid = event.agent as string;
    const sig = event.signal as string;
    if (sig === "permission_asked" && wid && aid) {
      const perWs = next.permissionPending[wid] ?? {};
      next.permissionPending = {
        ...next.permissionPending,
        [wid]: { ...perWs, [aid]: (event.permission_id as string) ?? "" },
      };
    }
    if (wid && aid && (sig === "session_idle" || sig === "step_started")) {
      const perWs = next.agentStates[wid] ?? {};
      next.agentStates = {
        ...next.agentStates,
        [wid]: {
          ...perWs,
          [aid]: sig === "step_started" ? "working" : "idle",
        },
      };
    }
  }

  return next;
}

/** Fold many events. */
export function reduceAll(initial: LiveState, events: BusEvent[]): LiveState {
  return events.reduce(reduce, initial);
}
