import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, parseGraph } from "../api/endpoints";
import { WorkflowCanvas } from "../components/WorkflowCanvas";
import { validateGraph, updateNode, addNode, type GraphIssue } from "../lib/graph-edit";
import type { GraphDef, NodeDef } from "../api/types";

const ROLE_PALETTE = ["dev", "reviewer", "tester", "designer", "memory-keeper"];

function useGraphNodeStates(graphId: string): Record<string, import("../api/types").NodeState> {
  const { data } = useQuery({
    queryKey: ["graphNodes", graphId],
    queryFn: () => api.graphNodes(graphId),
    refetchInterval: 2000,
  });
  return (data ?? []).reduce<Record<string, import("../api/types").NodeState>>((acc, row) => {
    acc[row.node_id] = row.state;
    return acc;
  }, {});
}

function Editor({ graph }: { graph: GraphDef }) {
  const [edit, setEdit] = useState<GraphDef>(graph);
  const [selected, setSelected] = useState<string | null>(null);
  const issues: GraphIssue[] = validateGraph(edit);
  const nodeStates = useGraphNodeStates(graph.id);
  const running = Object.values(nodeStates).some((s) => s === "running");

  const save = async () => {
    const result = await api.saveGraph(edit.id, JSON.stringify(edit, null, 2));
    void result;
  };

  const selectedNode = edit.nodes.find((n) => n.id === selected);

  const patch = (p: Partial<NodeDef>) => {
    if (!selected) return;
    setEdit((g) => updateNode(g, selected, p));
  };

  return (
    <div className="editor">
      <div className="editor-toolbar">
        <button disabled={issues.length > 0} onClick={() => void save()}>
          save
        </button>
        {running && <span className="badge-running">running — save applies to the next run</span>}
        {issues.length > 0 && (
          <span className="issues">{issues.map((i) => i.message).join("; ")}</span>
        )}
      </div>

      <div className="editor-body">
        <aside className="palette">
          <strong>palette</strong>
          {ROLE_PALETTE.map((role) => (
            <button
              key={role}
              onClick={() => {
                const id = `${role}_${edit.nodes.length + 1}`;
                setEdit((g) =>
                  addNode(g, {
                    id,
                    role,
                    depends_on: [],
                    start_template: `Do the ${role} task for {feature}.`,
                    done_when: { ack: id },
                    on_error: "delegate",
                    mode: "foreground",
                  }),
                );
                setSelected(id);
              }}
            >
              + {role}
            </button>
          ))}
        </aside>

        <div className="editor-canvas">
          <WorkflowCanvas
            graph={edit}
            mode="edit"
            onChange={setEdit}
            onNodeClick={(n) => setSelected(n.id)}
          />
        </div>

        {selectedNode && (
          <aside className="properties">
            <strong>{selectedNode.id}</strong>
            <label>
              role
              <input value={selectedNode.role} onChange={(e) => patch({ role: e.target.value })} />
            </label>
            <label>
              start_template
              <textarea
                value={selectedNode.start_template}
                onChange={(e) => patch({ start_template: e.target.value })}
              />
            </label>
            <label>
              done_when.ack
              <input
                value={selectedNode.done_when.ack ?? ""}
                onChange={(e) => patch({ done_when: { ...selectedNode.done_when, ack: e.target.value } })}
              />
            </label>
            <label>
              mode
              <select value={selectedNode.mode} onChange={(e) => patch({ mode: e.target.value as NodeDef["mode"] })}>
                <option value="foreground">foreground</option>
                <option value="background">background</option>
              </select>
            </label>
          </aside>
        )}
      </div>
    </div>
  );
}

export function Graphs({ id }: { id?: string }) {
  const { data: graphs } = useQuery({ queryKey: ["graphs"], queryFn: api.graphs, refetchInterval: 5000 });
  const selected = (graphs ?? []).find((g) => g.id === id);
  const selectedGraph = selected ? parseGraph(selected.data) : null;
  const liveNodes = useGraphNodeStates(id ?? "");

  return (
    <div className="page">
      <h1>graphs</h1>
      <ul className="graph-list">
        {(graphs ?? []).map((g) => (
          <li key={g.id}>
            <a href={`#/graphs/${g.id}`}>
              {g.id} <span className="dim">v{g.version} · {JSON.parse(g.data).nodes.length} nodes</span>
            </a>
          </li>
        ))}
      </ul>

      {selectedGraph && id && (
        <>
          <h2>{id} — live</h2>
          <div className="graph-live">
            <WorkflowCanvas graph={selectedGraph} mode="live" nodeStates={liveNodes} />
          </div>
          <h2>{id} — edit</h2>
          <Editor key={id} graph={selectedGraph} />
        </>
      )}
    </div>
  );
}
