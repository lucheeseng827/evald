import { useApi } from "../hooks";
import { type Score } from "../api";
import { Badge, Card, DataTable, EmptyState, Loading, Mono } from "../components";

// Aggregates stored `source=eval` numeric scores by evaluator name. Shared by the
// Overview tile and the Evals view — the console reads scores, the CLI runs evals.
export function EvalSummary({ scores }: { scores: Score[] }) {
  const evals = scores.filter((x) => x.source === "eval" && x.num_value != null);
  if (!evals.length) {
    return (
      <EmptyState title="No eval scores yet">
        Run <Mono>evald eval run --suite &lt;path&gt;</Mono> to populate deterministic Tier-1 + judge scores.
        They land as source <Mono>eval</Mono> and aggregate here.
      </EmptyState>
    );
  }
  const byName = new Map<string, number[]>();
  for (const e of evals) {
    const arr = byName.get(e.name) || [];
    arr.push(e.num_value as number);
    byName.set(e.name, arr);
  }
  const rows = [...byName.entries()].map(([name, vals]) => ({ name, n: vals.length, mean: vals.reduce((a, b) => a + b, 0) / vals.length }));
  return (
    <DataTable rows={rows} rowKey={(r) => r.name}
      columns={[
        { header: "Evaluator", width: "1.4fr", cell: (r) => <Mono>{r.name}</Mono> },
        { header: "n", width: "0.4fr", align: "right", cell: (r) => <Mono>{r.n}</Mono> },
        { header: "Mean", width: "0.6fr", align: "right", cell: (r) => <Badge tone={r.mean >= 0.8 ? "good" : r.mean >= 0.5 ? "warn" : "danger"}>{r.mean.toFixed(2)}</Badge> },
      ]} />
  );
}

export function Evals() {
  const { data, loading } = useApi<Score[]>("/v1/scores");
  return (
    <>
      <Card title="Eval scores by evaluator · aggregated over stored source=eval scores">
        {loading ? <Loading /> : <EvalSummary scores={data || []} />}
      </Card>
      <div className="grid split mt14" style={{ alignItems: "start" }}>
        <Card title="How eval runs land here">
          <p className="prose">The console reads scores from <Mono>GET /v1/scores</Mono> and aggregates the <Mono>source=eval</Mono> ones by evaluator. The regression loop itself is a CLI:</p>
          <div className="payload">{"evald eval run   --suite suite.yaml --out run_a\nevald eval compare run_a run_b   # Welch's t, α=0.05 gate"}</div>
          <p className="prose" style={{ marginTop: 12 }}>A run writes per-item scores plus a run-targeted aggregate carrying sufficient statistics (n, pass_count, mean, variance), so <Mono>eval compare</Mono> gates on a significant change rather than sampling noise.</p>
        </Card>
        <Card title="CI gate">
          <div className="row" style={{ marginBottom: 14 }}><Badge tone="default">exit 0 when no evaluator regresses significantly</Badge></div>
          <p className="prose">The gate fails only when the 95% CI on an evaluator's run-to-run delta excludes zero — an honest regression, not sampling noise.</p>
        </Card>
      </div>
    </>
  );
}
