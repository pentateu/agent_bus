import { describe, expect, it } from "vitest";
import {
  addNode,
  connect,
  disconnect,
  removeNode,
  removeNodes,
  updateNode,
  validateGraph,
} from "./graph-edit";
import type { GraphDef, NodeDef } from "../api/types";

const blank = (): GraphDef => ({ id: "g", name: "g", nodes: [] });

const node = (id: string, deps: string[] = []): NodeDef => ({
  id,
  role: "dev",
  depends_on: deps,
  start_template: "do it",
  done_when: { ack: id },
  on_error: "delegate",
  mode: "foreground",
});

describe("validateGraph", () => {
  it("accepts a valid chain", () => {
    const g: GraphDef = { ...blank(), nodes: [node("a"), node("b", ["a"])] };
    expect(validateGraph(g)).toEqual([]);
  });

  it("rejects duplicate ids", () => {
    const g: GraphDef = { ...blank(), nodes: [node("a"), node("a")] };
    expect(validateGraph(g).some((i) => i.message.includes("duplicate"))).toBe(true);
  });

  it("rejects a missing done_when criterion", () => {
    const n = { ...node("a"), done_when: {} };
    const g: GraphDef = { ...blank(), nodes: [n] };
    expect(validateGraph(g).some((i) => i.message.includes("done_when"))).toBe(true);
  });

  it("rejects an unknown dependency", () => {
    const g: GraphDef = { ...blank(), nodes: [node("a", ["ghost"])] };
    expect(validateGraph(g).some((i) => i.message.includes("unknown"))).toBe(true);
  });

  it("rejects a cycle", () => {
    const g: GraphDef = { ...blank(), nodes: [node("a", ["b"]), node("b", ["a"])] };
    expect(validateGraph(g).some((i) => i.message.includes("cycle"))).toBe(true);
  });

  it("rejects a bad loop_back target", () => {
    const n = { ...node("gate"), loop_back: { small: "gate", big: "missing" } };
    const g: GraphDef = { ...blank(), nodes: [n] };
    expect(validateGraph(g).some((i) => i.message.includes("loop_back"))).toBe(true);
  });
});

describe("edit helpers", () => {
  it("adds and removes nodes, pruning dependencies", () => {
    let g: GraphDef = { ...blank(), nodes: [node("a"), node("b", ["a"])] };
    g = addNode(g, node("c"));
    expect(g.nodes.length).toBe(3);
    g = removeNode(g, "a");
    expect(g.nodes.some((n) => n.id === "a")).toBe(false);
    expect(g.nodes.find((n) => n.id === "b")!.depends_on).toEqual([]);
  });

  it("batch-deletes all nodes in one sequential fold (M-3/F-2)", () => {
    let g: GraphDef = { ...blank(), nodes: [node("a"), node("b", ["a"]), node("c", ["b"])] };
    g = removeNodes(g, ["a", "b"]);
    expect(g.nodes.map((n) => n.id)).toEqual(["c"]);
    expect(g.nodes[0].depends_on).toEqual([]);
  });

  it("wires and unwires dependencies", () => {
    let g: GraphDef = { ...blank(), nodes: [node("a"), node("b")] };
    g = connect(g, "b", "a");
    expect(g.nodes.find((n) => n.id === "b")!.depends_on).toEqual(["a"]);
    g = connect(g, "b", "a");
    expect(g.nodes.find((n) => n.id === "b")!.depends_on).toEqual(["a"]);
    g = disconnect(g, "b", "a");
    expect(g.nodes.find((n) => n.id === "b")!.depends_on).toEqual([]);
  });

  it("updates node properties by id", () => {
    let g: GraphDef = { ...blank(), nodes: [node("a")] };
    g = updateNode(g, "a", { role: "reviewer" });
    expect(g.nodes[0].role).toBe("reviewer");
  });
});
