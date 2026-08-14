// Live state provider: bootstraps the token, subscribes once to the SSE
// stream, and feeds the pure reducer. REST data comes from react-query in the
// pages; live transitions come from here.

import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { bootstrapToken } from "../api/client";
import { streamEvents } from "../api/sse";
import { initialLiveState, reduce, type LiveState } from "./reduce";

const LiveContext = createContext<LiveState | null>(null);

export function LiveProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<LiveState>(initialLiveState);

  useEffect(() => {
    bootstrapToken();
  }, []);

  useEffect(() => {
    let cancelled = false;
    const pump = async () => {
      for await (const event of streamEvents()) {
        if (cancelled) return;
        setState((prev) => reduce(prev, event));
      }
    };
    void pump();
    return () => {
      cancelled = true;
    };
  }, []);

  void useMemo(() => state, [state]);

  return <LiveContext.Provider value={state}>{children}</LiveContext.Provider>;
}

export function useLive(): LiveState {
  const state = useContext(LiveContext);
  if (!state) throw new Error("useLive must be used within LiveProvider");
  return state;
}
