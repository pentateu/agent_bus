import { describe, expect, it } from "vitest";
import { initialLiveState, reduce, reduceAll } from "./reduce";
import type { BusEvent } from "../api/types";

const fleetAgent = (ws: string, agent: string, state: string): BusEvent => ({
  topic: "fleet",
  kind: "agent_state",
  workspace_id: ws,
  agent_id: agent,
  state,
});

describe("reduce", () => {
  it("tracks workspace state", () => {
    const e: BusEvent = {
      topic: "fleet",
      kind: "workspace_state",
      workspace: { id: "iot", state: "on" },
    } as BusEvent;
    const s = reduce(initialLiveState(), e);
    expect(s.workspaceStates["iot"]).toBe("on");
  });

  it("tracks agent state", () => {
    const s = reduce(initialLiveState(), fleetAgent("iot", "dev_01", "working"));
    expect(s.agentStates["iot"]["dev_01"]).toBe("working");
  });

  it("permission_asked sets the pending banner", () => {
    const e: BusEvent = {
      topic: "signal",
      signal: "permission_asked",
      ws: "iot",
      agent: "dev_01",
      permission_id: "p_9",
    } as BusEvent;
    const s = reduce(initialLiveState(), e);
    expect(s.permissionPending["iot"]["dev_01"]).toBe("p_9");
  });

  it("idle and step_started drive the agent state", () => {
    let s = initialLiveState();
    s = reduce(s, { topic: "signal", signal: "step_started", ws: "iot", agent: "a" } as BusEvent);
    expect(s.agentStates["iot"]["a"]).toBe("working");
    s = reduce(s, { topic: "signal", signal: "session_idle", ws: "iot", agent: "a" } as BusEvent);
    expect(s.agentStates["iot"]["a"]).toBe("idle");
  });

  it("keeps a bounded event ring", () => {
    const events: BusEvent[] = Array.from({ length: 250 }, () => fleetAgent("w", "a", "idle"));
    const s = reduceAll(initialLiveState(), events);
    expect(s.lastEvents.length).toBe(200);
  });
});
