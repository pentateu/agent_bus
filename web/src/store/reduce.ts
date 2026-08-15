// The pure live-state reducer (§6.4): `BusEvent[]` → `LiveState`. Unit-tested;
// drives the dashboard and every WorkflowCanvas. No network, no React.

import type { AgentState, BusEvent, WorkspaceState } from "../api/types";

export interface LiveState {
  workspaceStates: Record<string, WorkspaceState>;
  agentStates: Record<string, Record<string, AgentState>>;
  /** ws → agent → pending permission id (or null). */
  permissionPending: Record<string, Record<string, string | null>>;
  lastEvents: BusEvent[];
}

export function initialLiveState(): LiveState {
  return {
    workspaceStates: {},
    agentStates: {},
    permissionPending: {},
    lastEvents: [],
  };
}

const MAX_EVENTS = 200;

export function reduce(prev: LiveState, event: BusEvent): LiveState {
  const next: LiveState = {
    ...prev,
    workspaceStates: { ...prev.workspaceStates },
    agentStates: prev.agentStates,
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
  // F-8: workflow node states are NOT folded here — the bus events carry no
  // workspace_id (graph→workspace is ambiguous), and canvases read node
  // state by polling the workspace-scoped endpoint (documented in the spec).
  // `loop_back`/`missing_role` events are surfaced via that same polling.

  return next;
}

/** Fold many events. */
export function reduceAll(initial: LiveState, events: BusEvent[]): LiveState {
  return events.reduce(reduce, initial);
}
