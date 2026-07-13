import { useEffect, useState } from "react";
import { fmtNum, postSql, type SqlResponse } from "../api";
import { Card, DataTable, Faint, Loading, Mono } from "../components";

const GROUPS = {
  model: { col: "model", sql: "SELECT model, COUNT(*) spans, SUM(total_tokens) tokens, SUM(cost_usd) cost_usd FROM spans GROUP BY model ORDER BY tokens DESC" },
  provider: { col: "provider", sql: "SELECT provider, COUNT(*) spans, SUM(total_tokens) tokens, SUM(cost_usd) cost_usd FROM spans GROUP BY provider ORDER BY tokens DESC" },
  service: { col: "service_name", sql: "SELECT service_name, COUNT(*) spans, SUM(total_tokens) tokens, SUM(cost_usd) cost_usd FROM spans GROUP BY service_name ORDER BY tokens DESC" },
  user: { col: "user_id", sql: "SELECT user_id, COUNT(*) spans, SUM(total_tokens) tokens, SUM(cost_usd) cost_usd FROM spans GROUP BY user_id ORDER BY tokens DESC" },
} as const;
type GroupKey = keyof typeof GROUPS;

export function Cost() {
  const [by, setBy] = useState<GroupKey>("model");
  const [result, setResult] = useState<SqlResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setResult(null); setError(null);
    postSql(GROUPS[by].sql, 1000)
      .then((next) => { if (!cancelled) setResult(next); })
      .catch((e) => { if (!cancelled) setError((e as Error).message); });
    return () => { cancelled = true; };
  }, [by]);

  const col = GROUPS[by].col;
  return (
    <>
      <div className="row" style={{ marginBottom: 14 }}>
        <span className="muted" style={{ fontSize: 12 }}>Group by</span>
        <select className="select" style={{ width: 180 }} value={by} onChange={(e) => setBy(e.target.value as GroupKey)}>
          {Object.keys(GROUPS).map((k) => <option key={k} value={k}>{k}</option>)}
        </select>
      </div>
      <Card flush>
        {error ? <div className="payload" style={{ color: "var(--eg-danger)", margin: 16 }}>{error}</div>
          : !result ? <Loading />
          : result.rows.length ? (
            <DataTable rows={result.rows} rowKey={(_, i) => String(i)}
              columns={[
                { header: col, width: "1.6fr", cell: (r) => (r[col] == null ? <Faint>(untagged)</Faint> : <Mono>{String(r[col])}</Mono>) },
                { header: "spans", width: "0.7fr", align: "right", cell: (r) => <Mono>{fmtNum(r.spans as number)}</Mono> },
                { header: "tokens", width: "0.7fr", align: "right", cell: (r) => <Mono>{fmtNum(r.tokens as number)}</Mono> },
                { header: "cost_usd", width: "0.7fr", align: "right", cell: (r) => (r.cost_usd == null ? <Faint>—</Faint> : <Mono>${Number(r.cost_usd).toFixed(2)}</Mono>) },
              ]} />
          ) : <Loading label="no spans" />}
      </Card>
      <p className="hint">Untagged spans surface as <Mono>(untagged)</Mono> so partial tagging is visible. <Mono>cost_usd</Mono> shows when a span carried <Mono>llm.cost.*</Mono>; token totals are always available.</p>
    </>
  );
}
