import { useState } from "react";
import { useApi } from "../hooks";
import { fmtAgo, shortId, type Score, type ScoreSource } from "../api";
import { Badge, Card, DataTable, Faint, Loading, Mono, Tag, scoreTone, scoreValue } from "../components";

const FILTERS: (ScoreSource | "all")[] = ["all", "eval", "human", "api"];

export function Scores() {
  const { data, loading } = useApi<Score[]>("/v1/scores");
  const [filter, setFilter] = useState<ScoreSource | "all">("all");
  const rows = (data || []).filter((r) => filter === "all" || r.source === filter);
  return (
    <>
      <div className="row" style={{ marginBottom: 14 }}>
        {FILTERS.map((f) => (
          <button key={f} type="button" aria-pressed={filter === f} className="badge-button" onClick={() => setFilter(f)}>
            <Badge tone={filter === f ? "brand" : "default"}>{f}</Badge>
          </button>
        ))}
      </div>
      <Card title="Universal score store" flush>
        {loading ? <Loading /> : rows.length ? (
          <DataTable rows={rows} rowKey={(r) => r.id}
            columns={[
              { header: "Name", cell: (r) => <Mono>{r.name}</Mono> },
              { header: "Target", width: "1.1fr", cell: (r) => <div className="row"><Tag>{r.target_type}</Tag><Mono className="faint">{shortId(r.target_id)}</Mono></div> },
              { header: "Value", width: "0.7fr", cell: (r) => <Badge tone={scoreTone(r)}>{scoreValue(r)}</Badge> },
              { header: "Source", width: "0.6fr", cell: (r) => <Faint>{r.source}</Faint> },
              { header: "When", width: "0.7fr", align: "right", cell: (r) => <Faint>{fmtAgo(r.ts_unix_nano)}</Faint> },
            ]} />
        ) : <Loading label="no scores" />}
      </Card>
      <p className="hint">Phoenix clients POST to <Mono>/v1/span_annotations</Mono> — mapped onto span-targeted scores (HUMAN → source <Mono>human</Mono>). A non-empty <Mono>identifier</Mono> upserts.</p>
    </>
  );
}
