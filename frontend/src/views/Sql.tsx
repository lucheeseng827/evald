import { useRef, useState } from "react";
import { postSql, type SqlResponse } from "../api";
import { Card, DataTable, Loading, Mono } from "../components";

const EXAMPLES = [
  { label: "tokens by model", q: "SELECT model, COUNT(*) n, SUM(total_tokens) tok\nFROM spans GROUP BY model ORDER BY tok DESC" },
  { label: "denied evals", q: "SELECT s.name, s.model FROM spans s\nJOIN scores sc ON sc.target_id = s.span_id\nWHERE sc.name = 'exact_match' AND sc.num_value = 0" },
];
const DEFAULT_SQL =
  "SELECT s.model, AVG(sc.num_value) AS score, COUNT(*) AS n\nFROM spans s\nJOIN scores sc ON sc.target_id = s.span_id\nGROUP BY s.model\nORDER BY score DESC";

export function Sql() {
  const [sql, setSql] = useState(DEFAULT_SQL);
  const [result, setResult] = useState<SqlResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const requestId = useRef(0);

  async function run() {
    const id = ++requestId.current;
    setError(null);
    try {
      const next = await postSql(sql, 1000);
      if (id === requestId.current) setResult(next);
    } catch (e) {
      if (id === requestId.current) { setError((e as Error).message); setResult(null); }
    }
  }

  return (
    <>
      <Card flush>
        <div className="row" style={{ padding: "12px 16px", borderBottom: "1px solid var(--eg-border-soft)" }}>
          <button className="btn primary sm" onClick={run}>Run</button>
          <span className="faint" style={{ fontSize: 12 }}>⌘/Ctrl + Enter</span>
          <span className="spacer" />
          {EXAMPLES.map((ex) => <button key={ex.label} className="btn sm mono" onClick={() => setSql(ex.q)}>{ex.label}</button>)}
        </div>
        <textarea className="textarea mono" spellCheck={false} style={{ border: "none", borderRadius: 0 }}
          value={sql} onChange={(e) => setSql(e.target.value)}
          onKeyDown={(e) => { if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); run(); } }} />
      </Card>
      <div className="hint">{result ? `${result.rows.length} rows${result.truncated ? " · truncated" : ""}` : " "}</div>
      <Card flush><SqlResult result={result} error={error} /></Card>
    </>
  );
}

export function SqlResult({ result, error }: { result: SqlResponse | null; error: string | null }) {
  if (error) return <div className="payload" style={{ color: "var(--eg-danger)" }}>{error}</div>;
  if (!result) return <Loading label="Run a query." />;
  if (!result.rows.length) return <Loading label="0 rows" />;
  return (
    <DataTable rows={result.rows} rowKey={(_, i) => String(i)}
      columns={result.columns.map((c) => ({ header: c, cell: (row: Record<string, unknown>) => <Mono>{row[c] == null ? "—" : String(row[c])}</Mono> }))} />
  );
}
