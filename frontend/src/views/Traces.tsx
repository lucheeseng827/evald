import { useEffect, useMemo, useState } from "react";
import { api, fmtAgo, fmtDur, fmtNum, type Score, type Span } from "../api";
import { Badge, Card, DataTable, Faint, Loading, Mono, Tag, scoreTone, scoreValue } from "../components";

interface TraceRow { id: string; name: string; model: string; count: number; tokens: number; start: number; }

function groupTraces(spans: Span[]): TraceRow[] {
  const map = new Map<string, { spans: Span[]; root: Span | null; tokens: number; start: number }>();
  for (const sp of spans) {
    let t = map.get(sp.trace_id);
    if (!t) { t = { spans: [], root: null, tokens: 0, start: 0 }; map.set(sp.trace_id, t); }
    t.spans.push(sp);
    t.tokens += sp.tokens?.total || 0;
    if (sp.parent_span_id == null || !t.root) t.root = sp;
    t.start = Math.max(t.start, sp.start_unix_nano || 0);
  }
  return [...map.entries()]
    .map(([id, t]) => ({
      id, name: t.root?.name || "(trace)", model: t.root?.model || t.spans.find((s) => s.model)?.model || "",
      count: t.spans.length, tokens: t.tokens, start: t.start,
    }))
    .sort((a, b) => b.start - a.start);
}

// full-text haystack + AND-term matcher (kept from the prior console)
const haystack = (t: TraceRow) => [t.name, t.model, t.id].filter(Boolean).join(" ").toLowerCase();
const traceMatches = (t: TraceRow, terms: string[]) => terms.every((term) => haystack(t).includes(term));

const spanKind = (s: Span) => s.oi_kind || ["UNSPEC", "INTERNAL", "SERVER", "CLIENT", "PRODUCER", "CONSUMER"][s.otel_kind] || "SPAN";

export function Traces() {
  const [spans, setSpans] = useState<Span[]>([]);
  const [scores, setScores] = useState<Score[]>([]);
  const [search, setSearch] = useState("");
  const [traceId, setTraceId] = useState<string | null>(null);
  const [traceSpans, setTraceSpans] = useState<Span[]>([]);
  const [spanId, setSpanId] = useState<string | null>(null);

  useEffect(() => {
    api<Span[]>("/v1/spans").then(setSpans).catch(() => {});
    api<Score[]>("/v1/scores").then(setScores).catch(() => {});
  }, []);

  const traces = useMemo(() => groupTraces(spans), [spans]);
  useEffect(() => {
    if (!traceId && traces.length) setTraceId(traces[0].id);
  }, [traces, traceId]);
  useEffect(() => {
    if (!traceId) return;
    let cancelled = false;
    setSpanId(null);
    setTraceSpans([]);
    api<Span[]>("/v1/traces/" + encodeURIComponent(traceId))
      .then((next) => { if (!cancelled) setTraceSpans(next); })
      .catch(() => { if (!cancelled) setTraceSpans([]); });
    return () => { cancelled = true; };
  }, [traceId]);

  const scoresByTarget = useMemo(() => {
    const m = new Map<string, Score[]>();
    for (const s of scores) { const a = m.get(s.target_id) || []; a.push(s); m.set(s.target_id, a); }
    return m;
  }, [scores]);

  const terms = search.toLowerCase().split(/\s+/).filter(Boolean);
  const shown = traces.filter((t) => traceMatches(t, terms));
  const selectedTrace = traces.find((t) => t.id === traceId);
  const sel = traceSpans.find((s) => s.span_id === spanId) || null;

  return (
    <div className="grid" style={{ gridTemplateColumns: "360px 1fr", alignItems: "start" }}>
      <section className="card flush">
        <div className="card-head">
          <span className="card-title">Traces</span>
          <span className="faint mono" style={{ fontSize: 12 }}>{traces.length}</span>
        </div>
        <div style={{ padding: "10px 12px" }}>
          <input id="trace-search" className="input mono" type="search" placeholder="Filter traces…  name · model · id"
            autoComplete="off" spellCheck={false} value={search} onChange={(e) => setSearch(e.target.value)} />
        </div>
        <div style={{ maxHeight: 460, overflowY: "auto" }}>
          {shown.length === 0 ? <Loading label="no traces" /> : shown.map((t) => (
            <button key={t.id} className={"nav-item" + (t.id === traceId ? " active" : "")}
              style={{ flexDirection: "column", alignItems: "stretch", gap: 3, padding: "10px 12px" }} onClick={() => setTraceId(t.id)}>
              <div style={{ color: "var(--eg-fg)", fontWeight: 500, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{t.name}</div>
              <div className="mono" style={{ fontSize: 11, color: "var(--eg-faint)" }}>{[t.model, fmtNum(t.tokens) + " tok", fmtAgo(t.start)].filter(Boolean).join(" · ")}</div>
            </button>
          ))}
        </div>
      </section>

      <div className="stack">
        <SpanTree trace={selectedTrace} spans={traceSpans} spanId={spanId} onSelect={setSpanId} />
        {sel && <SpanDetail span={sel} scores={scoresByTarget.get(sel.span_id) || []} />}
      </div>
    </div>
  );
}

function depthOf(span: Span, byId: Map<string, Span>): number {
  let d = 0, cur: Span | undefined = span, guard = 0;
  while (cur && cur.parent_span_id && byId.get(cur.parent_span_id) && guard++ < 64) { d++; cur = byId.get(cur.parent_span_id!); }
  return d;
}

function SpanTree({ trace, spans, spanId, onSelect }: { trace?: TraceRow; spans: Span[]; spanId: string | null; onSelect: (id: string) => void }) {
  const byId = useMemo(() => { const m = new Map<string, Span>(); for (const s of spans) m.set(s.span_id, s); return m; }, [spans]);
  const rows = spans.map((s) => ({ ...s, _depth: depthOf(s, byId) }));
  return (
    <Card title={trace?.name || "Trace"}>
      {trace && <div className="mono" style={{ fontSize: 11, color: "var(--eg-faint)", margin: "-4px 0 12px" }}>{trace.id}</div>}
      {spans.length ? (
        <DataTable rows={rows} rowKey={(r) => r.span_id} onRowClick={(r) => onSelect(r.span_id)} selKey={(r) => r.span_id === spanId}
          columns={[
            { header: "Span", width: "1.5fr", cell: (r) => (
              <div className="row" style={{ paddingLeft: r._depth * 16, gap: 8, flexWrap: "nowrap" }}>
                <Tag>{spanKind(r)}</Tag>
                <span style={{ color: "var(--eg-fg)", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{r.name}</span>
                {r.orphan_parent && <Badge tone="warn">orphan</Badge>}
              </div>) },
            { header: "Dur", width: "0.5fr", align: "right", cell: (r) => <Mono>{fmtDur(r.start_unix_nano, r.end_unix_nano)}</Mono> },
            { header: "Tokens", width: "0.5fr", align: "right", cell: (r) => (r.tokens?.total ? <Mono>{fmtNum(r.tokens.total)}</Mono> : <Faint>—</Faint>) },
          ]} />
      ) : <Loading label="select a trace" />}
    </Card>
  );
}

function SpanDetail({ span, scores }: { span: Span; scores: Score[] }) {
  const attrs: [string, string][] = [];
  if (span.model) attrs.push(["gen_ai.request.model", span.model]);
  if (span.provider) attrs.push(["provider", span.provider]);
  if (span.service_name) attrs.push(["service.name", span.service_name]);
  if (span.session_id) attrs.push(["session.id", span.session_id]);
  if (span.user_id) attrs.push(["user.id", span.user_id]);
  if (span.tokens) for (const [k, v] of Object.entries(span.tokens)) if (v != null) attrs.push(["tokens." + k, fmtNum(v as number)]);
  if (span.cost_usd != null) attrs.push(["cost_usd", "$" + span.cost_usd.toFixed(4)]);
  for (const [k, v] of Object.entries(span.raw_attributes || {})) attrs.push([k, typeof v === "object" ? JSON.stringify(v) : String(v)]);

  return (
    <Card title={span.name}>
      <div className="row" style={{ margin: "-2px 0 14px" }}>
        <Tag>{spanKind(span)}</Tag>
        <Tag>{fmtDur(span.start_unix_nano, span.end_unix_nano)}</Tag>
        {span.model && <Tag>{span.model}</Tag>}
      </div>
      <div className="grid k2">
        <div>
          <div className="sub-label">Attributes</div>
          {attrs.map(([k, v], i) => <div key={i} className="kv"><span className="k">{k}</span><span className="v">{v}</span></div>)}
        </div>
        <div>
          <div className="sub-label">Scores on span</div>
          {scores.length ? (
            <div className="stack" style={{ gap: 8 }}>
              {scores.map((sc) => (
                <div key={sc.id} className="row" style={{ justifyContent: "space-between", padding: "8px 10px", border: "1px solid var(--eg-border-soft)", borderRadius: 8 }}>
                  <Mono>{sc.name}</Mono>
                  <div className="row"><Faint>{sc.source}</Faint><Badge tone={scoreTone(sc)}>{scoreValue(sc)}</Badge></div>
                </div>
              ))}
            </div>
          ) : <div className="faint" style={{ fontSize: 13 }}>No scores yet. POST /v1/scores to attach one.</div>}
        </div>
      </div>
      {span.input_value && <PayloadBlock label="input" value={span.input_value} />}
      {span.output_value && <PayloadBlock label="output" value={span.output_value} />}
    </Card>
  );
}

// Payload virtualization: large values truncate with an expand toggle; an
// `evald-blob:<key>` reference links to GET /v1/blobs/<key>.
const PAYLOAD_CAP = 2000;
function PayloadBlock({ label, value }: { label: string; value: string }) {
  const [expanded, setExpanded] = useState(false);
  const blob = /^evald-blob:(.+)$/.exec(value.trim());
  return (
    <div>
      <div className="sub-label" style={{ marginTop: 16 }}>{label}</div>
      {blob ? (
        <div className="payload">
          <span className="blob-ref">evald-blob:{blob[1]} </span>
          <a href={"/v1/blobs/" + encodeURIComponent(blob[1])} target="_blank" rel="noreferrer">fetch offloaded payload →</a>
        </div>
      ) : (
        <>
          <div className="payload">{expanded || value.length <= PAYLOAD_CAP ? value : value.slice(0, PAYLOAD_CAP)}</div>
          {value.length > PAYLOAD_CAP && (
            <button type="button" className="payload-more" onClick={() => setExpanded((e) => !e)}>
              {expanded ? "collapse" : `show ${fmtNum(value.length - PAYLOAD_CAP)} more chars`}
            </button>
          )}
        </>
      )}
    </div>
  );
}
