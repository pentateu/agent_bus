// The reusable WorkflowCanvas (§6): one renderer, two modes.
// - `live`: nodeStates + agentStates drive colors/animations; positions from
//   dagre; click-to-open only.
// - `edit`: React Flow manages positions; wiring fires `onChange`.
// Pure component — no network calls.

import { useEffect, useMemo } from "react";
import {
  Background,
  Controls,
  Handle,
  Position,
  ReactFlow,
  type Edge,
  type Node,
  type NodeProps,
  useEdgesState,
  useNodesState,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import type { AgentState, GraphDef, NodeDef, NodeState } from "../api/types";
import { layoutGraph } from "../lib/layout";
import { connect, disconnect, removeNode } from "../lib/graph-edit";

const ROLE_GLYPH: Record<string, string> = {
  dev: "⚙️",
  reviewer: "🔍",
  tester: "🧪",
  designer: "🎨",
  "memory-keeper": "📚",
  manager: "🧭",
};

const NODE_W = 180;
const NODE_H = 64;

export interface WorkflowCanvasProps {
  graph: GraphDef;
  mode: "edit" | "live";
  nodeStates?: Record<string, NodeState>;
  agentStates?: Record<string, AgentState>;
  onNodeClick?: (node: NodeDef, agentId?: string) => void;
  onChange?: (graph: GraphDef) => void;
  compact?: boolean;
}

type CardData = {
  label: string;
  role: string;
  state: NodeState;
  agent?: string;
  agentState?: AgentState;
} & Record<string, unknown>;

function StateCard({ data }: NodeProps<Node<CardData>>) {
  const d = data;
  const glyph = ROLE_GLYPH[d.role] ?? "▫️";
  const missing = d.state === "pending" && !d.agent;
  return (
    <div className={`wf-node wf-${d.state}${missing ? " wf-missing" : ""}`}>
      <Handle type="target" position={Position.Top} />
      <div className="wf-glyph">{glyph}</div>
      <div className="wf-id">{d.label}</div>
      <div className="wf-meta">
        {d.agent ?? (missing ? "no agent" : "—")}
        {d.agentState && <span className={`wf-dot wf-agent-${d.agentState}`} title={d.agentState} />}
      </div>
      {d.state === "running" && <div className="wf-spinner" />}
      <Handle type="source" position={Position.Bottom} />
    </div>
  );
}

function toFlow(
  graph: GraphDef,
  states: Record<string, NodeState>,
  agentStates: Record<string, AgentState>,
  positions?: Array<{ id: string; x: number; y: number }>,
): { nodes: Node<CardData>[]; edges: Edge[] } {
  const pos = positions ? new Map(positions.map((p) => [p.id, p])) : new Map();
  const nodes: Node<CardData>[] = graph.nodes.map((n, i) => {
    const p = pos.get(n.id);
    return {
      id: n.id,
      type: "stateCard",
      position: p ? { x: p.x, y: p.y } : { x: 40 + (i % 3) * 220, y: 40 + Math.floor(i / 3) * 110 },
      data: {
        label: n.id,
        role: n.role,
        state: states[n.id] ?? "pending",
        agent: n.agent_id ?? undefined,
        agentState: n.agent_id ? agentStates[n.agent_id] : undefined,
      },
    };
  });
  const edges: Edge[] = graph.nodes.flatMap((n) =>
    n.depends_on.map((dep) => ({
      id: `${dep}-${n.id}`,
      source: dep,
      target: n.id,
      animated: states[dep] === "done" && states[n.id] === "ready",
      style: { strokeWidth: 1.5 },
    })),
  );
  return { nodes, edges };
}

export function WorkflowCanvas(props: WorkflowCanvasProps) {
  const { graph, mode, nodeStates, agentStates, onNodeClick, onChange, compact } = props;
  const states = nodeStates ?? {};
  const agentStateMap = agentStates ?? {};

  const layout = useMemo(() => (mode === "live" ? layoutGraph(graph, NODE_W, NODE_H) : undefined), [graph, mode]);
  const live = mode === "live";

  // Live: fully derived from props (re-renders on SSE state changes).
  const liveNodes = useMemo(
    () => toFlow(graph, states, agentStateMap, layout).nodes,
    [graph, states, agentStateMap, layout],
  );
  const liveEdges = useMemo(
    () => toFlow(graph, states, agentStateMap, layout).edges,
    [graph, states, agentStateMap, layout],
  );

  // Edit: React Flow owns positions; we only re-seed when the graph structure
  // changes.
  const editSeed = useMemo(() => toFlow(graph, states, agentStateMap, undefined), [graph, states, agentStateMap]);
  const [nodes, setNodes, onNodesChange] = useNodesState(editSeed.nodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(editSeed.edges);
  useEffect(() => {
    if (live) return;
    setNodes(editSeed.nodes);
    setEdges(editSeed.edges);
  }, [editSeed, live, setNodes, setEdges]);

  const nodeTypes = useMemo(() => ({ stateCard: StateCard }), []);

  const onConnectEdit = (conn: { source?: string; target?: string }) => {
    if (!onChange || !conn.source || !conn.target) return;
    onChange(connect(graph, conn.target, conn.source));
  };
  // C-6: backspace removal used to update only React Flow's internal state —
  // the parent graph kept the node, it reappeared on the next re-seed, and
  // save persisted the "deleted" node. Route `remove` changes to onChange.
  const onNodesChangeEdit: import("@xyflow/react").OnNodesChange<Node<CardData>> = (changes) => {
    onNodesChange(changes);
    if (!onChange) return;
    for (const change of changes) {
      if (change.type === "remove" && change.id) {
        onChange(removeNode(graph, change.id));
      }
    }
  };
  const onEdgesDeleteEdit = (deleted: Edge[]) => {
    if (!onChange) return;
    let next = graph;
    for (const edge of deleted) next = disconnect(next, edge.target ?? "", edge.source ?? "");
    onChange(next);
  };
  const handleNodeClick = (_: unknown, n: Node<CardData>) => {
    onNodeClick?.(graph.nodes.find((g) => g.id === n.id)!, n.data.agent);
  };

  return (
    <div className={`wf-canvas${compact ? " wf-compact" : ""}`}>
      <ReactFlow
        nodes={live ? liveNodes : nodes}
        edges={live ? liveEdges : edges}
        nodeTypes={nodeTypes}
        onNodesChange={live ? undefined : onNodesChangeEdit}
        onEdgesChange={live ? undefined : onEdgesChange}
        onConnect={live ? undefined : onConnectEdit}
        onEdgesDelete={live ? undefined : onEdgesDeleteEdit}
        nodesDraggable={!live}
        nodesConnectable={!live}
        fitView
        proOptions={{ hideAttribution: true }}
        onNodeClick={handleNodeClick}
      >
        <Background />
        <Controls showInteractive={!live} />
      </ReactFlow>
    </div>
  );
}

/** The agent-state pill (used in canvases + dashboard). */
export function AgentChip({ agent, state }: { agent: string; state?: AgentState }) {
  return <span className={`wf-agent-chip wf-agent-${state ?? "unknown"}`}>{agent}</span>;
}
