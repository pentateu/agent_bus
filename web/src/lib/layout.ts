// Layered DAG layout via dagre (§6.1: live/view mode + editor auto-arrange).

import dagre from "dagre";
import type { GraphDef } from "../api/types";

export interface PositionedNode {
  id: string;
  x: number;
  y: number;
}

/**
 * Compute layered positions for a graph. Returns absolute top-left positions
 * for a node card of `nodeW`×`nodeH`. The graph JSON stays positions-free; this
 * is derived every render (live mode) or on "auto-arrange" (edit mode).
 */
export function layoutGraph(graph: GraphDef, nodeW = 180, nodeH = 64): PositionedNode[] {
  const g = new dagre.graphlib.Graph();
  g.setDefaultEdgeLabel(() => ({}));
  g.setGraph({ rankdir: "TB", nodesep: 40, ranksep: 70, marginx: 10, marginy: 10 });
  for (const n of graph.nodes) g.setNode(n.id, { width: nodeW, height: nodeH });
  for (const n of graph.nodes) for (const dep of n.depends_on) g.setEdge(dep, n.id);
  dagre.layout(g);
  return graph.nodes.map((n) => {
    const pos = g.node(n.id) as { x: number; y: number };
    return { id: n.id, x: pos.x - nodeW / 2, y: pos.y - nodeH / 2 };
  });
}
