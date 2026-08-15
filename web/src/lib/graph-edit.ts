// Pure graph-editing helpers (§5.3, U4): add/remove/wire/unwire and a
// client-side validation that mirrors `supervisor-core`'s checks.

import type { GraphDef, NodeDef } from "../api/types";

export type GraphIssue = { node?: string; message: string };

/** Deep-cycle detection via DFS coloring. */
function findCycle(graph: GraphDef): string[] | null {
  const index = new Map<string, NodeDef>(graph.nodes.map((n) => [n.id, n]));
  const color = new Map<string, 0 | 1 | 2>();
  const stack: string[] = [];
  const visit = (id: string): string[] | null => {
    color.set(id, 1);
    stack.push(id);
    const node = index.get(id);
    if (!node) {
      // Unknown dependency: validation already reported it; don't descend.
      stack.pop();
      color.set(id, 2);
      return null;
    }
    for (const dep of node.depends_on) {
      const c = color.get(dep);
      if (c === 1) return stack.slice(stack.indexOf(dep));
      if (c === undefined) {
        const cycle = visit(dep);
        if (cycle) return cycle;
      }
    }
    stack.pop();
    color.set(id, 2);
    return null;
  };
  for (const n of graph.nodes) {
    if (!color.get(n.id)) {
      const cycle = visit(n.id);
      if (cycle) return cycle;
    }
  }
  return null;
}

/** Validate a graph with the same rules as `supervisor-core` (§4.11). */
export function validateGraph(graph: GraphDef): GraphIssue[] {
  const issues: GraphIssue[] = [];
  const ids = new Set<string>();
  for (const n of graph.nodes) {
    if (!n.id) issues.push({ message: "a node id must not be empty" });
    if (ids.has(n.id)) issues.push({ node: n.id, message: `duplicate node id "${n.id}"` });
    ids.add(n.id);
    const hasCriterion = n.done_when?.ack || n.done_when?.match;
    if (!hasCriterion) {
      issues.push({ node: n.id, message: `node "${n.id}" has no done_when criterion` });
    }
  }
  for (const n of graph.nodes) {
    for (const dep of n.depends_on) {
      if (!ids.has(dep)) {
        issues.push({ node: n.id, message: `node "${n.id}" depends on unknown "${dep}"` });
      }
    }
    const lb = n.loop_back;
    if (lb) {
      for (const target of [lb.small, lb.big]) {
        if (!ids.has(target)) {
          issues.push({ node: n.id, message: `loop_back target "${target}" does not exist` });
        }
      }
    }
  }
  const cycle = findCycle(graph);
  if (cycle) issues.push({ message: `dependency cycle: ${cycle.join(" → ")}` });
  return issues;
}

export function addNode(graph: GraphDef, node: NodeDef): GraphDef {
  return { ...graph, nodes: [...graph.nodes, node] };
}

export function removeNode(graph: GraphDef, id: string): GraphDef {
  return {
    ...graph,
    nodes: graph.nodes
      .filter((n) => n.id !== id)
      .map((n) => ({ ...n, depends_on: n.depends_on.filter((d) => d !== id) })),
  };
}

/**
 * Remove several nodes in one fold, applied sequentially to the accumulated
 * graph (review M-3/F-2: applying each removal to the same stale `graph`
 * resurrected all but the last node in a batch delete).
 */
export function removeNodes(graph: GraphDef, ids: readonly string[]): GraphDef {
  let next = graph;
  for (const id of ids) next = removeNode(next, id);
  return next;
}

/** Wire `from → to` (add `to` to `from`'s deps). No-op when already wired. */
export function connect(graph: GraphDef, from: string, to: string): GraphDef {
  return {
    ...graph,
    nodes: graph.nodes.map((n) =>
      n.id === from && !n.depends_on.includes(to)
        ? { ...n, depends_on: [...n.depends_on, to] }
        : n,
    ),
  };
}

export function disconnect(graph: GraphDef, from: string, to: string): GraphDef {
  return {
    ...graph,
    nodes: graph.nodes.map((n) =>
      n.id === from ? { ...n, depends_on: n.depends_on.filter((d) => d !== to) } : n,
    ),
  };
}

export function updateNode(graph: GraphDef, id: string, patch: Partial<NodeDef>): GraphDef {
  return {
    ...graph,
    nodes: graph.nodes.map((n) => (n.id === id ? { ...n, ...patch, id } : n)),
  };
}
