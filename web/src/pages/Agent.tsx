import { useEffect, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { api } from "../api/endpoints";
import { useLive } from "../store/live-store";
import type { TranscriptMessage } from "../api/types";

export function AgentDialog({ ws, agent }: { ws: string; agent: string }) {
  const live = useLive();
  const [compose, setCompose] = useState("");
  const [high, setHigh] = useState(false);

  const { data: agents } = useQuery({ queryKey: ["agents", ws], queryFn: () => api.agents(ws) });
  const meta = (agents ?? []).find((a) => a.agent_id === agent);
  const state = live.agentStates[ws]?.[agent] ?? meta?.state ?? "unknown";
  const pendingPermission = live.permissionPending[ws]?.[agent];

  const { data: transcript } = useQuery({
    queryKey: ["transcript", ws, agent],
    queryFn: () => api.transcript(ws, agent, 50),
    refetchInterval: state === "working" ? 1500 : 10000,
  });

  const send = useMutation({
    mutationFn: (body: string) => api.sendMessage(ws, agent, body, high ? "high" : "normal"),
  });
  const abort = useMutation({ mutationFn: () => api.abortAgent(ws, agent) });
  const attach = useMutation({ mutationFn: () => api.attachAgent(ws, agent) });
  const permission = useMutation({
    mutationFn: ({ pid, allow }: { pid: string; allow: boolean }) =>
      api.respondPermission(ws, agent, pid, allow ? "allow" : "deny", true),
  });

  const rows = (transcript ?? []) as TranscriptMessage[];

  // Auto-scroll to the newest row.
  useEffect(() => {
    const el = document.querySelector(".transcript");
    if (el) el.scrollTop = el.scrollHeight;
  }, [rows.length]);

  return (
    <div className="page agent-page">
      <header className="agent-header">
        <a href={`#/workspaces/${ws}`} className="dim">← {ws}</a>
        <h1>
          {agent} <span className={`agent-state wf-agent-${state}`}>{state}</span>
        </h1>
        <div className="dim">
          role {meta?.role} · {meta?.mode} · driver {meta?.driver}
          {meta?.session_id ? ` · ${meta.session_id}` : ""}
        </div>
      </header>

      {pendingPermission && (
        <div className="permission-banner">
          <strong>Permission requested</strong> ({pendingPermission})
          <button onClick={() => permission.mutate({ pid: pendingPermission, allow: true })}>Allow</button>
          <button onClick={() => permission.mutate({ pid: pendingPermission, allow: false })}>Deny</button>
        </div>
      )}

      <div className="transcript">
        {rows.length === 0 && <p className="dim">No transcript yet.</p>}
        {rows.map((m, i) => (
          <div key={i} className={`row row-${m.role}`}>
            <span className="row-role">{m.role}</span>
            <span className="row-ts">{m.ts}</span>
            <pre>{m.text}</pre>
          </div>
        ))}
      </div>

      <div className="actions">
        <button disabled={abort.isPending} onClick={() => abort.mutate()}>
          abort
        </button>
        <button disabled={attach.isPending} onClick={() => attach.mutate()}>
          attach pane
        </button>
        {meta?.mode === "background" && <span className="dim">currently headless</span>}
        {attach.data && <span className="dim">{attach.data.spawned ? "pane spawned" : attach.data.attach}</span>}
      </div>

      <form
        className="compose"
        onSubmit={(e) => {
          e.preventDefault();
          if (!compose.trim()) return;
          send.mutate(compose);
          setCompose("");
        }}
      >
        <input
          value={compose}
          onChange={(e) => setCompose(e.target.value)}
          placeholder={`message ${agent}…`}
        />
        <label className="high">
          <input type="checkbox" checked={high} onChange={(e) => setHigh(e.target.checked)} />
          high
        </label>
        <button type="submit">send</button>
      </form>
    </div>
  );
}
