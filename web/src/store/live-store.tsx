// Live state provider: bootstraps the token, subscribes once to the SSE
// stream, and feeds the pure reducer. REST data comes from react-query in the
// pages; live transitions come from here.

import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { bootstrapToken } from "../api/client";
import { streamEvents } from "../api/sse";
import { initialLiveState, reduce, type LiveState } from "./reduce";

const LiveContext = createContext<LiveState | null>(null);
const ClearPermissionContext = createContext<(ws: string, agent: string) => void>(() => {});

export function LiveProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<LiveState>(initialLiveState);

  useEffect(() => {
    bootstrapToken();
  }, []);

  useEffect(() => {
    // I-24: unmount must abort the SSE stream — a `cancelled` flag alone
    // leaves the generator's reconnect loop running forever.
    const controller = new AbortController();
    const pump = async () => {
      for await (const event of streamEvents(controller.signal)) {
        setState((prev) => reduce(prev, event));
      }
    };
    void pump();
    return () => {
      controller.abort();
    };
  }, []);

  void useMemo(() => state, [state]);

  return (
    <LiveContext.Provider value={state}>
      <ClearPermissionContext.Provider
        value={(ws, agent) =>
          setState((prev) => {
            const perWs = { ...prev.permissionPending[ws] };
            delete perWs[agent];
            return { ...prev, permissionPending: { ...prev.permissionPending, [ws]: perWs } };
          })
        }
      >
        {children}
      </ClearPermissionContext.Provider>
    </LiveContext.Provider>
  );
}

export function useLive(): LiveState {
  const state = useContext(LiveContext);
  if (!state) throw new Error("useLive must be used within LiveProvider");
  return state;
}

/** I-27: clear a resolved/denied permission banner (the signal stream has no
 * "permission resolved" event, so the UI clears on mutation success). */
export function useClearPermission(): (ws: string, agent: string) => void {
  return useContext(ClearPermissionContext);
}
