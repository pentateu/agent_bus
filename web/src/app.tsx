import { useEffect, useState } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LiveProvider, useLive } from "./store/live-store";
import { hasToken, onTokenChange } from "./api/client";
import { Dashboard } from "./pages/Dashboard";
import { AgentDialog } from "./pages/Agent";
import { Graphs } from "./pages/Graphs";
import { Decisions } from "./pages/Decisions";

const queryClient = new QueryClient();

function parseRoute(): { page: string; ws?: string; agent?: string; graph?: string } {
  const hash = window.location.hash.replace(/^#\/?/, "");
  const parts = hash.split("/").filter(Boolean);
  if (parts[0] === "workspaces" && parts[1]) {
    if (parts[2] === "agents" && parts[3]) return { page: "agent", ws: parts[1], agent: parts[3] };
    return { page: "workspace", ws: parts[1] };
  }
  if (parts[0] === "graphs" && parts[1]) return { page: "graph", graph: parts[1] };
  if (parts[0] === "graphs") return { page: "graphs" };
  if (parts[0] === "decisions") return { page: "decisions" };
  return { page: "dashboard" };
}

function Shell() {
  const [route, setRoute] = useState(parseRoute);
  // F-4: re-render when the token clears (401) so the missing-token gate
  // appears immediately instead of on the next nav/SSE event.
  const [tokenVersion, setTokenVersion] = useState(0);
  useEffect(() => onTokenChange(() => setTokenVersion((v) => v + 1)), []);
  useEffect(() => {
    const onChange = () => setRoute(parseRoute());
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, []);
  void useLive();
  void tokenVersion;

  if (!hasToken()) {
    return (
      <div className="app missing-token">
        <h1>supervisor</h1>
        <p>This UI needs the loopback token.</p>
        <p>
          Run <code>supervisor web</code> to open it with the token in the URL.
        </p>
      </div>
    );
  }

  const nav = (label: string, target: string) => (
    <a className={route.page === target ? "active" : ""} href={`#/${target}`}>
      {label}
    </a>
  );

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">supervisor</span>
        <nav>
          {nav("Dashboard", "dashboard")}
          {nav("Graphs", "graphs")}
          {nav("Decisions", "decisions")}
        </nav>
      </header>
      <main>
        {route.page === "dashboard" && <Dashboard />}
        {route.page === "workspace" && route.ws && <Dashboard ws={route.ws} />}
        {route.page === "agent" && route.ws && route.agent && <AgentDialog ws={route.ws} agent={route.agent} />}
        {route.page === "graphs" && <Graphs />}
        {route.page === "graph" && route.graph && <Graphs id={route.graph} />}
        {route.page === "decisions" && <Decisions />}
      </main>
    </div>
  );
}

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <LiveProvider>
        <Shell />
      </LiveProvider>
    </QueryClientProvider>
  );
}
