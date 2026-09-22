import { useApi } from "../hooks";
import { api, fmtNum, postSql, type IngestStats, type Score, type Span, type SqlResponse } from "../api";
import { Badge, Card, DataTable, Kpi, Loading, Meter, Mono, Faint, scoreTone, scoreValue } from "../components";
import { useEffect, useState } from "react";
import { EvalSummary } from "./Evals";

// The same grouping the Cost view offers, pinned to model and capped at the few rows
// that fit here — this card used to hold nothing but a sentence pointing at that view.
const COST_BY_MODEL =
  "SELECT model, COUNT(*) spans, SUM(total_tokens) tokens, SUM(cost_usd) cost_usd FROM spans GROUP BY model ORDER BY tokens DESC";

export function Overview() {
  const stats = useApi<IngestStats>("/v1/stats");
  const [spans, setSpans] = useState<Span[]>([]);
  const [scores, setScores] = useState<Score[]>([]);
  const [cost, setCost] = useState<SqlResponse | null>(null);
  const [costFailed, setCostFailed] = useState(false);
  useEffect(() => {
    api<Span[]>("/v1/spans").then(setSpans).catch(() => {});
    api<Score[]>("/v1/scores").then(setScores).catch(() => {});
    postSql(COST_BY_MODEL, 5).then(setCost).catch(() => setCostFailed(true));
  }, []);

  const s = stats.data;
  const traces = new Set(spans.map((x) => x.trace_id)).size;
  const evalN = scores.filter((x) => x.source === "eval").length;
  const humanN = scores.filter((x) => x.source === "human").length;

  return (
    <>
      <div className="grid k4">
        <Kpi label="Spans (hot)" value={s ? fmtNum(s.hot_spans) : "…"} foot={s ? "resident in hot tier" : ""} />
        <Kpi label="Traces" value={fmtNum(traces)} foot="hot ∪ cold, deduped" />
        <Kpi label="Scores attached" value={fmtNum(scores.length)} accent foot={`eval ${fmtNum(evalN)} · human ${fmtNum(humanN)}`} />
        <Kpi label="Ingest" value={s ? (s.shedding ? "shedding" : "healthy") : "…"} foot={s ? `${fmtNum(s.rejections)} sheds cumulative` : ""} />
      </div>

      <div className="grid split mt14">
        <Card title="Ingest pipeline">
          <div className="row" style={{ marginBottom: 16 }}>
            <Badge tone={s?.shedding ? "danger" : "good"} dot>{s?.shedding ? "shedding" : "not shedding"}</Badge>
            <Badge tone="good" dot>WAL durable</Badge>
            <Badge tone="default">{`${s ? fmtNum(s.rejections) : 0} rejections`}</Badge>
          </div>
          {s ? (
            <Meter label="Hot-tier headroom" value={s.hot_spans} max={s.max_hot_spans || 1_000_000}
              valueLabel={`${fmtNum(s.hot_spans)} / ${s.max_hot_spans ? fmtNum(s.max_hot_spans) : "∞"}`} tone={s.shedding ? "danger" : "good"} />
          ) : <Loading />}
          <div className="row" style={{ marginTop: 18 }}>
            <div>
              <div className="muted" style={{ fontSize: 12 }}>Channel capacity</div>
              <div style={{ fontSize: 22, fontWeight: 700 }}>{s ? fmtNum(s.channel_capacity) : "—"}</div>
            </div>
          </div>
        </Card>

        <Card title="Eval scores by evaluator"><EvalSummary scores={scores} /></Card>
      </div>

      <div className="grid k2 mt14">
        <Card title="Cost by model" flush>
          {costFailed ? <Loading label="cost unavailable — /v1/sql did not answer" />
            : cost === null ? <Loading />
            : cost.rows.length ? (
              <DataTable rows={cost.rows} rowKey={(_, i) => String(i)}
                columns={[
                  { header: "Model", cell: (r) => (r.model == null ? <Faint>(untagged)</Faint> : <Mono>{String(r.model)}</Mono>) },
                  { header: "Tokens", width: "0.7fr", align: "right", cell: (r) => <Mono>{fmtNum(r.tokens as number)}</Mono> },
                  { header: "Cost", width: "0.6fr", align: "right", cell: (r) => (r.cost_usd == null ? <Faint>—</Faint> : <Mono>${Number(r.cost_usd).toFixed(2)}</Mono>) },
                ]} />
            ) : <Loading label="no spans yet" />}
        </Card>
        <Card title="Recent scores" flush>
          {scores.length ? (
            <DataTable rows={scores.slice(0, 6)} rowKey={(r) => r.id}
              columns={[
                { header: "Name", cell: (r) => <Mono>{r.name}</Mono> },
                { header: "Src", width: "0.5fr", cell: (r) => <Faint>{r.source}</Faint> },
                { header: "Value", width: "0.6fr", align: "right", cell: (r) => <Badge tone={scoreTone(r)}>{scoreValue(r)}</Badge> },
              ]} />
          ) : <Loading label="no scores yet" />}
        </Card>
      </div>
    </>
  );
}
