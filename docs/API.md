# evald HTTP API reference

Every endpoint `evald serve` exposes, on one listener (default
`127.0.0.1:4318`). Source of truth: the axum router in
[`src/ingest.rs`](../src/ingest.rs) (`build_router`); regenerate this file when
it changes.

**Authentication: none — by design.** The OSS core is unauthenticated and binds
**loopback** by default; the threat model is a laptop or a locked-down CI runner.
Before exposing `:4318` beyond localhost, put it behind a reverse proxy that adds
authn/z and TLS (or use the separately-licensed ee fleet layer). See
[OPERATIONS.md § Security posture](./OPERATIONS.md#security-posture).

Errors are plain text (`evald: <message>\n`) unless noted. Cross-cutting
responses:

| Status | When | Body / headers |
|---|---|---|
| `429 Too Many Requests` | ingest channel full (overload shed — never a silent drop) | `Retry-After: 1`, `evald: overloaded, retry shortly` |
| `413 Payload Too Large` | request body over 16 MiB **decompressed** (the cap is applied after gzip inflation) | axum default body |
| `503 Service Unavailable` | the store writer failed (WAL write/fsync error, shutdown) | `evald: store unavailable` |

---

## POST /v1/traces

OTLP/HTTP trace export — point any OpenInference/OTel SDK at the server
(`OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318`). Accepts both wire
formats; the reply mirrors the request format. `Content-Encoding: gzip` bodies
are inflated transparently.

- `Content-Type: application/x-protobuf` — OTLP protobuf (`ExportTraceServiceRequest`).
- `Content-Type: application/json` — OTLP-JSON (int64-as-string fields like
  `intValue` are accepted per the protobuf JSON mapping, numeric spellings too).

The response is only sent **after** the spans are fsynced to the WAL — a `200`
means durable.

```bash
curl -s http://localhost:4318/v1/traces \
  -H 'Content-Type: application/json' \
  -d '{"resourceSpans":[{"scopeSpans":[{"spans":[{
        "traceId":"0123456789abcdef0123456789abcdef","spanId":"0123456789abcdef",
        "name":"chat","kind":3,
        "startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000500000000",
        "attributes":[{"key":"gen_ai.request.model","value":{"stringValue":"gpt-4o"}}]}]}]}]}'
# -> {}    (an empty ExportTraceServiceResponse = full success)
```

| Status | Meaning |
|---|---|
| `200` | all spans normalized + durably stored (empty `partial_success`). |
| `400` | body did not decode as OTLP protobuf / OTLP-JSON (message says which). |
| `413` / `429` / `503` | see the cross-cutting table. |

## GET /v1/spans

Recent spans (hot ∪ cold, deduped by `(trace_id, span_id)`), most-recent-first,
as an array of normalized-span JSON objects.

Query params: `trace_id` (optional filter), `limit` (default 100, clamped to
`[1, 10000]`).

```bash
curl -s 'http://localhost:4318/v1/spans?limit=1'
# -> [{"dialect":"mixed","trace_id":"abab…","span_id":"cdcd…","name":"openai.chat.completions",
#      "oi_kind":"LLM","model":"claude-opus-4-8","provider":"anthropic",
#      "tokens":{"prompt":1875,"completion":432,"total":2307},"service_name":"demo-app", …}]
```

`500` with `evald: query failed` on a store read error.

## GET /v1/traces/{trace_id}

All spans of one trace, in chronological (start-time) order.

| Status | Meaning |
|---|---|
| `200` | JSON array of the trace's spans. |
| `404` | `evald: no spans for that trace_id`. |

## GET /v1/stats

Ingest-pipeline load: the in-memory hot-tier backlog, the shed/backpressure counters, and
the ingest channel depth — the signal for when the store is approaching its shed threshold.
The embedded SPA's Dashboard reads this shape.

```bash
curl -s http://localhost:4318/v1/stats
# -> {"hot_spans":128,"max_hot_spans":1000000,"channel_capacity":1024,"rejections":0,"shedding":false}
```

| Field | Meaning |
|---|---|
| `hot_spans` | Un-compacted spans resident in the hot tier (memory the compactor must drain). |
| `max_hot_spans` | The configured hot-tier bound (`0` = unbounded). |
| `channel_capacity` | Ingest channel depth (in-flight append commands before a channel-full shed). |
| `rejections` | Cumulative spans shed because the hot tier was at its bound. |
| `shedding` | `true` once `hot_spans ≥ max_hot_spans` — ingest is currently shedding (`429 + Retry-After`). |

## POST /v1/scores

Upsert one score or an array of scores (evald-native shape). A score needs
**one target id** — the most specific present wins: `span_id` > `trace_id` >
`session_id` > `run_id` — and **a value**: `value` (number / string / bool,
type inferred) or explicit `num_value` / `str_value`.

Optional fields: `id` (upsert key; else a fresh UUID), `data_type`
(`numeric` | `categorical` | `boolean` | `text`), `source`
(`eval` | `human` | `api`; default `api`), `comment`, `config_id`,
`ts_unix_nano`.

```bash
curl -s http://localhost:4318/v1/scores \
  -d '{"span_id":"cdcdcdcdcdcdcdcd","name":"exact_match","value":1}'
# -> 201  {"ids":["c4f649a2-55c4-4630-858f-a5240d603584"]}
```

| Status | Meaning |
|---|---|
| `201` | stored; body `{"ids":[…]}` in input order. |
| `400` | invalid JSON, missing target, or missing value (message says which). |
| `500` | `evald: could not store scores`. |

## GET /v1/scores

Scores for a target, or recent scores across all targets when no target param is
given. Query params: `span_id` / `trace_id` / `session_id` / `run_id` (first
present wins, same precedence as POST), `limit` (no-target listing only; default
100, clamped to `[1, 10000]`).

```bash
curl -s 'http://localhost:4318/v1/scores?span_id=cdcdcdcdcdcdcdcd'
# -> [{"id":"…","target_type":"span","target_id":"cdcdcdcdcdcdcdcd","name":"exact_match",
#      "num_value":1.0,"data_type":"numeric","source":"api","ts_unix_nano":…}]
```

## GET /v1/scores/{id}

One score by id — `200` with the score object, or `404`
(`evald: no score with that id`).

## POST /v1/span_annotations

**Phoenix-compatible** span-annotation endpoint, so Phoenix REST clients work
unchanged. Envelope: `{"data":[{"span_id","name","annotator_kind","result":
{"label","score","explanation"},"identifier"}]}`. Each annotation is mapped onto
a span-targeted Score (`annotator_kind: "HUMAN"` → source `human`; `LLM`/`CODE` →
`eval`; extra fields like `metadata` are accepted and ignored). A non-empty
`identifier` derives a stable id (`<span_id>:<name>:<identifier>`) so re-posting
**upserts** — Phoenix semantics.

```bash
curl -s http://localhost:4318/v1/span_annotations \
  -d '{"data":[{"span_id":"cdcdcdcdcdcdcdcd","name":"correctness","annotator_kind":"HUMAN",
       "result":{"label":"correct","score":1.0,"explanation":"looks right"},"identifier":"rev-1"}]}'
# -> 200  {"data":[{"id":"cdcdcdcdcdcdcdcd:correctness:rev-1"}]}
```

| Status | Meaning |
|---|---|
| `200` | `{"data":[{"id":…}]}`, in input order. |
| `422` | malformed body — mirrors Phoenix/FastAPI's validation contract: `{"detail":[{"loc","msg","type"}]}` (not evald's plain-text 400), so Phoenix clients' error handling works unchanged. |
| `500` | `evald: could not store annotations`. |

## POST /v1/sql

Read-only DataFusion SQL over the store. Registered tables:

- `spans` — a view over the hot tier ∪ the committed cold Parquet blocks,
  deduped by `(trace_id, span_id)` (hot wins), so a query never double-counts or
  misses a span across an in-flight compaction. Columns = the flat Parquet schema
  ([`src/store/cold.rs`](../src/store/cold.rs) `schema()`): `trace_id`,
  `span_id`, `name`, `oi_kind`, `model`, `provider`, `prompt_tokens`,
  `completion_tokens`, `total_tokens`, `cost_usd`, `input_value`,
  `output_value`, `session_id`, `user_id`, `service_name`,
  `raw_attributes_json` (JSON string), ….
- `scores` — the score store; `target_id` joins to `spans.span_id`
  (the OTel-native join).

Body: `{"sql": "...", "limit": 1000}` — `limit` caps the **result** (default
1 000, max 100 000; `truncated: true` when the tail was dropped), not the scan.

**Write guard:** only a single read statement is accepted — a `SELECT`/`WITH`
query or a plain `EXPLAIN` (not `EXPLAIN ANALYZE`, which executes). The check is
on the **parsed statement type**, not a leading keyword, so `INSERT`, `COPY TO`,
`CREATE EXTERNAL TABLE`, and `EXPLAIN ANALYZE INSERT …` are all rejected with
`400`. ([`src/sql.rs`](../src/sql.rs) `ensure_read_only` is the canonical guard.)

```bash
curl -s http://localhost:4318/v1/sql \
  -d '{"sql":"SELECT s.model, AVG(sc.num_value) score FROM spans s JOIN scores sc ON sc.target_id = s.span_id GROUP BY s.model"}'
# -> {"columns":["model","score"],"rows":[{"model":"claude-opus-4-8","score":1.0}],
#     "row_count":1,"truncated":false}

curl -s http://localhost:4318/v1/sql -d '{"sql":"INSERT INTO spans VALUES (1)"}'
# -> 400  evald: SQL error: only read queries are allowed — SELECT / WITH, or EXPLAIN (without ANALYZE) over a read query
```

| Status | Meaning |
|---|---|
| `200` | `{columns, rows, row_count, truncated}` — rows are JSON objects keyed by column name. |
| `400` | invalid body, non-read statement, SQL parse/plan/execution error (message included, so the SPA console can show it). |

Note: queries are ad-hoc full scans over the blocks — cheap at laptop scale, but
consider the scan cost before pointing dashboards at a large store
(see [BENCHMARKS.md](./BENCHMARKS.md)).

## Everything else — the embedded SPA

Any path not matched above falls through to the embedded single-page app
(rust-embed; trace list → trace tree → scores, plus a SQL console) with an
`index.html` fallback for client-side routes. It is registered as the router
**fallback**, so it can never shadow a `/v1/*` API route. Works air-gapped —
all assets are compiled into the binary.

```bash
curl -s -o /dev/null -w '%{http_code} %{content_type}\n' http://localhost:4318/
# -> 200 text/html; charset=utf-8
```
