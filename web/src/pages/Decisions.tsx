import { useMutation, useQuery } from "@tanstack/react-query";
import { api } from "../api/endpoints";

export function Decisions() {
  const { data: decisions } = useQuery({ queryKey: ["decisions"], queryFn: api.decisions, refetchInterval: 5000 });
  const { data: proposals } = useQuery({ queryKey: ["proposals"], queryFn: api.proposals, refetchInterval: 5000 });
  const apply = useMutation({ mutationFn: (id: string) => api.applyProposal(id) });
  const reject = useMutation({ mutationFn: (id: string) => api.rejectProposal(id) });
  const preview = useMutation({ mutationFn: () => api.previewBakeback() });

  return (
    <div className="page">
      <h1>decisions</h1>

      <section>
        <h2>
          bake-back proposals{" "}
          <button onClick={() => preview.mutate()}>preview</button>
        </h2>
        {(proposals ?? []).length === 0 && <p className="dim">no proposals</p>}
        {(proposals ?? []).map((p) => (
          <div className="proposal" key={p.id}>
            <code>{p.id}</code> · cluster {p.cluster_size} · conf {p.confidence.toFixed(2)} ·{" "}
            <span className={p.status}>{p.status}</span>
            {p.status === "pending" && (
              <>
                <button onClick={() => apply.mutate(p.id)}>apply</button>
                <button onClick={() => reject.mutate(p.id)}>reject</button>
              </>
            )}
            <pre>{p.rule_toml}</pre>
          </div>
        ))}
      </section>

      <section>
        <h2>decision log</h2>
        {(decisions ?? []).length === 0 && <p className="dim">no decisions yet</p>}
        {(decisions ?? []).map((d) => (
          <div className="decision" key={d.id}>
            <span className="row-ts">{d.ts}</span>{" "}
            <code>{d.signature}</code>{" "}
            <span className="dim">outcome: {d.outcome ? JSON.stringify(d.outcome) : "—"}</span>
          </div>
        ))}
      </section>
    </div>
  );
}
