import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, parseGraph } from "../api/endpoints";
import { useLive } from "../store/live-store";
import { AgentChip, WorkflowCanvas } from "../components/WorkflowCanvas";
import type { Agent, GraphRecord, NodeState } from "../api/types";

function Metric({ label, value, est }: { label: string; value: string; est?: boolean }) {
  return (
    <div className="metric">
      <span className="metric-label">{label}</span>
      <span className="metric-value">
        {value}
        {est ? " est." : ""}
      </span>
    </div>
  );
}

function useGraphNodeStates(ws: string, graphId: string): Record<string, NodeState> {
  const { data } = useQuery({
    queryKey: ["graphNodes", ws, graphId],
    queryFn: () => api.graphNodes(ws, graphId),
    refetchInterval: 2000,
  });
  return (data ?? []).reduce<Record<string, NodeState>>((acc, row) => {
    acc[row.node_id] = row.state;
    return acc;
  }, {});
}

/** A live canvas for one installed graph (hook-safe child). Full-width, one
 * per workspace at a time (the workspace card tabs between them). */
function LiveGraph({ ws, graph, agents }: { ws: string; graph: GraphRecord; agents: Agent[] }) {
  const live = useLive();
  const nodeStates = useGraphNodeStates(ws, graph.id);
  const parsed = parseGraph(graph.data);
  const agentStates = agents.reduce<Record<string, import("../api/types").AgentState>>((acc, a) => {
    acc[a.agent_id] = live.agentStates[ws]?.[a.agent_id] ?? a.state;
    return acc;
  }, {});
  return (
    <div className="ws-canvas">
      <a className="canvas-title" href={`#/graphs/${graph.id}`}>
        {graph.id}
      </a>
      <WorkflowCanvas
        graph={parsed}
        mode="live"
        nodeStates={nodeStates}
        agentStates={agentStates}
        onNodeClick={(n, agent) => {
          if (agent) window.location.hash = `#/workspaces/${ws}/agents/${agent}`;
          else void n;
        }}
      />
    </div>
  );
}

function WorkspaceCard({ ws }: { ws: string }) {
  const live = useLive();
  const [activeGraph, setActiveGraph] = useState<string | null>(null);
  const { data: agents } = useQuery({
    queryKey: ["agents", ws],
    queryFn: () => api.agents(ws),
    refetchInterval: 3000,
  });
  const { data: graphs } = useQuery({ queryKey: ["graphs"], queryFn: api.graphs });
  const agentList = agents ?? [];
  const running = (graphs ?? []).filter((g) => g.active);
  // One process at a time: tabs when several are active, defaulting to the
  // first. If the selected graph vanishes, fall back to the first active one.
  const current = running.find((g) => g.id === activeGraph) ?? running[0];
  const state = live.workspaceStates[ws] ?? "off";
  const triage = agentList.filter((a) => a.state === "waiting_input" || a.state === "blocked_permission");
  // I-28: lifecycle mutations must surface failures, not vanish.
  const [wsError, setWsError] = useState<string | null>(null);
  const toggle = async (next: "on" | "off") => {
    setWsError(null);
    try {
      if (next === "on") await api.workspaceOn(ws);
      else await api.workspaceOff(ws, true);
    } catch (e) {
      setWsError(`${next} failed: ${(e as Error).message}`);
    }
  };

  return (
    <div className={`ws-card ws-${state}`}>
      <div className="ws-header">
        <a href={`#/workspaces/${ws}`} className="ws-name">
          {ws}
        </a>
        <span className="ws-state">{state}</span>
        {state === "off" && <button onClick={() => void toggle("on")}>on</button>}
        {state === "on" && <button onClick={() => void toggle("off")}>off</button>}
      </div>
      {wsError && (
        <div className="ws-triage" role="alert">
          {wsError}
        </div>
      )}
      <div className="ws-agents">
        {agentList.map((a) => (
          <a key={a.agent_id} href={`#/workspaces/${ws}/agents/${a.agent_id}`}>
            <AgentChip agent={a.agent_id} state={live.agentStates[ws]?.[a.agent_id] ?? a.state} />
          </a>
        ))}
        {agentList.length === 0 && <span className="dim">no agents configured</span>}
      </div>
      {triage.length > 0 && <div className="ws-triage">⚠ {triage.length} awaiting input/approval</div>}
      {running.length > 1 && (
        <div className="graph-tabs" role="tablist" aria-label={`${ws} workflows`}>
          {running.map((g) => (
            <button
              key={g.id}
              role="tab"
              aria-selected={g.id === current?.id}
              className={`graph-tab${g.id === current?.id ? " active" : ""}`}
              onClick={() => setActiveGraph(g.id)}
            >
              {g.id}
            </button>
          ))}
        </div>
      )}
      {current && <LiveGraph key={current.id} ws={ws} graph={current} agents={agentList} />}
      {running.length === 0 && <p className="dim">no active graphs</p>}
    </div>
  );
}

export function Dashboard({ ws }: { ws?: string }) {
  const { data: metrics } = useQuery({
    queryKey: ["metrics"],
    queryFn: () => api.metrics(),
    refetchInterval: 5000,
  });
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: api.workspaces,
    refetchInterval: 3000,
  });
  const totals = metrics?.totals;

  return (
    <div className="page">
      <section className="metrics-strip">
        <Metric label="messages" value={String(totals?.messages_delivered ?? "—")} />
        <Metric label="errors" value={String(totals?.errors ?? "—")} />
        <Metric label="decisions" value={String(totals?.decisions ?? "—")} />
        <Metric label="nodes done" value={String(totals?.nodes_done ?? "—")} />
        <Metric label="tokens" value={String(totals?.tokens ?? "—")} />
        <Metric
          label="est. cost"
          value={totals?.cost_cents != null ? `$${((totals.cost_cents ?? 0) / 100).toFixed(2)}` : "—"}
          est
        />
      </section>

      {(workspaces ?? []).filter((w) => !ws || w.id === ws).length === 0 && (
        <p className="empty">
          No workspaces yet — run <code>supervisor add &lt;path&gt;</code>.
        </p>
      )}

      <div className="ws-grid">
        {(workspaces ?? [])
          .filter((w) => !ws || w.id === ws)
          .map((w) => (
            <WorkspaceCard key={w.id} ws={w.id} />
          ))}
      </div>
    </div>
  );
}
