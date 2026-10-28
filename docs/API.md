# evald HTTP API reference

Every endpoint `evald serve` exposes, on one listener (default
`127.0.0.1:4318`). Source of truth: the axum router in
[`src/ingest.rs`](../src/ingest.rs) (`build_router`); regenerate this file when
it changes.

**Authentication: optional, OFF by default.** The OSS core binds **loopback** by
default and, with no token configured, is unauthenticated — the threat model is a
laptop or a locked-down CI runner. To expose `:4318`/`:4317` beyond localhost, either
(a) arm the built-in bearer-token gate with `--auth-token` / `EVALD_AUTH_TOKEN` /
`--auth-token-file` (see below), or (b) put it behind a reverse proxy that adds
authn/z and TLS, or (c) use the separately-licensed ee fleet layer. See
[OPERATIONS.md § Security posture](./OPERATIONS.md#security-posture).

When the gate is armed, **every** request (OTLP ingest, all `/v1/*`, and the SPA)
must carry `Authorization: Bearer <token>`; a missing or wrong token is `401`. This
is a shared-secret gate, **not** TLS — terminate TLS at a proxy if the transport is
untrusted.

```bash
# a token added to every call — curl, an OTLP exporter, a CI step
curl -s http://HOST:4318/v1/spans -H "Authorization: Bearer $EVALD_TOKEN"
# OTLP SDKs: OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer $EVALD_TOKEN"
```

Errors are plain text (`evald: <message>\n`) unless noted. Cross-cutting
responses:

| Status | When | Body / headers |
|---|---|---|
| `401 Unauthorized` | auth gate armed and the request had no / a wrong `Authorization: Bearer <token>` | `WWW-Authenticate: Bearer`, `evald: unauthorized — set Authorization: Bearer <token>` |
| `429 Too Many Requests` | ingest channel full (overload shed — never a silent drop) | `Retry-After: 1`, `evald: overloaded, retry shortly` |
| `413 Payload Too Large` | request body over 16 MiB **decompressed** (the cap is applied after gzip inflation) | axum default body |
| `503 Service Unavailable` | the data-dir filesystem is below `--disk-min-free` | `Retry-After: 30`, `evald: out of disk headroom, retry later`. Distinct from the `429` above: a backlog drains on its own, so the client should slow down; a full disk is neither the client's fault nor within its power to fix, and clears only when space is freed or retention runs. |
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

**Evaluation events.** A span event named `gen_ai.evaluation.result` in the export is also
stored as a [score](#post-v1scores) (`source: eval`) targeting the span the event is attached
to, on the HTTP protobuf, OTLP-JSON and OTLP/gRPC receivers alike. The mapping:

| Event attribute | Score field |
|---|---|
| `gen_ai.evaluation.name` (required) | `name` |
| `gen_ai.evaluation.score.value` | `num_value` (`data_type: numeric`). A non-numeric or non-finite value is ignored. |
| `gen_ai.evaluation.score.label` | `str_value` (`data_type: categorical` when there is no numeric value). |
| `gen_ai.evaluation.explanation` | `comment` |
| `error.type` | appended to `comment`; an event with only an `error.type` is stored as the label `error` |
| event time | `ts_unix_nano` (the span's end time when the event carries none) |

The score `id` is a hash of the trace, span, evaluation name and event time, so an exporter
retry of the same batch overwrites the score instead of duplicating it. An event with no
evaluation name, or with none of a numeric value, a label or an `error.type`, is dropped and
counted in `evald_eval_events_malformed_total`; the spans in the same request are stored
regardless. Names, labels and error types are capped at 256 bytes and explanations at 4 KiB.
`gen_ai.response.id` is not stored. The same event sent as an OTLP **log record** is ingested by
[`POST /v1/logs`](#post-v1logs), with the same mapping.

This follows OpenTelemetry semantic-conventions-genai at commit `cc07f72` (2026-09-21;
semantic conventions v1.44.0), where the event is at **Development** stability. The attribute
names may change upstream; the pin above is what this build implements. Read them back with
`evald scores export --format gen_ai-event` ([CONFIG.md](./CONFIG.md#evald-scores-export)).

## POST /v1/logs

OTLP/HTTP logs export, the signal coding agents use for their events. The formats, gzip
handling, durability and replies are those of [`POST /v1/traces`](#post-v1traces); the
OTLP/gRPC `LogsService` on the gRPC listener takes the same requests. Point an exporter's logs
at it with `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=http://localhost:4318/v1/logs`, or set
`OTEL_EXPORTER_OTLP_ENDPOINT` and every signal finds its path.

evald is not a log store. A record is kept only when it is recognised, and then it becomes an
ordinary span or score:

| Record | Becomes |
|---|---|
| A `gen_ai.evaluation.result` event | a score, mapped [as the span event is](#post-v1traces), targeting the span the record's trace context names (or its trace, without a span id) |
| A Claude Code event (scope `com.anthropic.claude_code.events`) | a span: `api_request` an LLM span with tokens and Claude Code's own cost, `tool_result` a TOOL span, and so on. The table is in [INSTRUMENTATION.md § Claude Code](./INSTRUMENTATION.md#claude-code) |
| A Codex CLI event (scope `codex_otel.*`, service `codex*`) | a span: a stream's `response.completed` an LLM span with tokens, `tool_result` a TOOL span, and so on ([INSTRUMENTATION.md § Codex CLI](./INSTRUMENTATION.md#codex-cli)) |
| A Gemini CLI event (service `gemini-cli`) | a span: `api_response` an LLM span with tokens and thinking tokens, `tool_call` a TOOL span, and so on ([INSTRUMENTATION.md § Gemini CLI](./INSTRUMENTATION.md#gemini-cli)) |
| Anything else | dropped and counted, unless `--logs all` ([CONFIG.md](./CONFIG.md#evald-serve)) keeps it as a zero-duration span carrying its attributes plus `log.body`, `log.severity_text` and `log.severity_number` |

A record's event name is read from the `event.name` attribute, then the OTLP `event_name`
field, then a string body. The attribute comes first because the field is not always a name:
Rust's `opentelemetry-appender-tracing` fills it with a `tracing` callsite (Codex does).
An attribute sent as an empty value (`"value": {}`, which Node's JSON exporters send for an
undefined one) is read as no value. Every received record is counted in
`evald_log_records_total{outcome}`, and the outcomes add up to the records received.

| Status | Meaning |
|---|---|
| `200` | every record processed. A record evald does not keep is still acknowledged: dropping it is the policy, not a failure an exporter should retry. |
| `400` | body did not decode as OTLP protobuf / OTLP-JSON. |
| `413` / `429` / `503` | see the cross-cutting table. |

**One call, one span.** Claude Code with tracing on reports every API call twice, as an event
here and as a span on `/v1/traces`. Both copies get the same span id, and the second to
arrive is dropped at ingest (`evald_duplicate_calls_dropped_total`), so cost, tokens and the
usage series count the call once. What this does and does not cover is in
[INSTRUMENTATION.md § Claude Code](./INSTRUMENTATION.md#claude-code).

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

## GET /v1/sessions/{session_id}

One session rolled up: what an agent or a chat did across its traces. Spans are matched on
`session_id` (`session.id`, or `gen_ai.conversation.id`).

```bash
curl -s http://localhost:4318/v1/sessions/6cefe7a6-bc13-5da8-a6cd-980ff9a20596
# -> {"session_id":"6cefe7a6-…","span_count":33,"trace_count":3,"cost_usd":0.01167107,
#     "tokens":{"prompt":262,"completion":962,"total":443549,"cache_read":392877,"cache_write":49448},
#     "start_unix_nano":…,"end_unix_nano":…,
#     "llm_calls":14,"llm_errors":0,"tool_calls":6,"tool_errors":0,
#     "models":["claude-haiku-5-5"],"tools":{"Bash":6},"scanned":33,"truncated":false}
```

| Field | Meaning |
|---|---|
| `span_count`, `trace_count` | Spans and distinct traces (one trace per turn, for Claude Code). |
| `cost_usd`, `tokens` | Summed over the spans that carry them; `cost_usd` is absent when none did. |
| `start_unix_nano`, `end_unix_nano` | Earliest start and latest end: the session's wall-clock extent. |
| `llm_calls`, `llm_errors` | LLM spans, and those that ended in an error status. |
| `tool_calls`, `tool_errors` | **Distinct tool calls**, by `gen_ai.tool.call.id` (or an alias) when a span carries one, so a call reported on two signals counts once; a tool span without an id under a tool span of the same tool (a framework's span and the MCP `tools/call` span inside it) is the same call. A call fails if any copy has an error status; a refused tool is a failure. |
| `models`, `tools` | Models called (sorted), and tool calls by tool name. |
| `agents` | Agents that ran (`gen_ai.agent.name`, or OpenInference's `agent.name`), sorted; absent when none named itself. |
| `steps_per_trace` | Model calls per trace: how many steps a turn took. |
| `tool_error_rate` | `tool_errors / tool_calls`; absent without tool calls. A call with an `error.type` (an MCP `isError` result) is a failure too. |
| `repeated_tool_calls`, `longest_repeat_run`, `loop_suspected` | Tool calls identical to the call just before them in the same trace (same tool, same arguments), the longest run of identical calls, and whether it reached 3: a suspected loop. Only calls that carry their arguments can repeat (`gen_ai.tool.call.arguments` or an alias; Claude Code sends them with `OTEL_LOG_TOOL_DETAILS=1`), so a tool that reports none never raises a false alarm. |
| `scanned`, `truncated` | The rollup reads the most recent 50,000 spans (there is no session index yet). `truncated: true` means the scan hit that cap and older spans of the session may be missing. |

| Status | Meaning |
|---|---|
| `200` | The rollup. |
| `404` | `evald: no spans for that session_id` (among the spans scanned). |

## GET /v1/sessions

The most recently active sessions, each rolled up as above, newest first. `?limit=` (default 20,
at most 1000). It is what `evald sessions --json` prints, for a store a running server holds
(the CLI cannot open the store while the server has it: the lock is exclusive).

```bash
curl -s 'http://localhost:4318/v1/sessions?limit=5'
# -> [{"session_id":"…","llm_calls":14,"tool_calls":6,…}, …]
```

## GET /v1/stats

Ingest-pipeline load: the in-memory hot-tier backlog, the shed/backpressure counters, and
the ingest channel depth — the signal for when the store is approaching its shed threshold.
The embedded SPA's Dashboard reads this shape.

```bash
curl -s http://localhost:4318/v1/stats
# -> {"hot_spans":128,"max_hot_spans":1000000,"channel_capacity":1024,"rejections":0,
#     "shedding":false,"spans_ingested":4096,"compactions":12,"compaction_failures":0}
```

| Field | Meaning |
|---|---|
| `hot_spans` | Un-compacted spans resident in the hot tier (memory the compactor must drain). |
| `max_hot_spans` | The configured hot-tier bound (`0` = unbounded). |
| `channel_capacity` | Ingest channel depth (in-flight append commands before a channel-full shed). |
| `rejections` | Cumulative spans shed because the hot tier was at its bound. |
| `shedding` | `true` once `hot_spans ≥ max_hot_spans` — ingest is currently shedding (`429 + Retry-After`). |
| `spans_ingested` | Cumulative spans durably ACK'd since process start (counted at the committing fsync). |
| `compactions` | Background compaction passes completed since process start. |
| `compaction_failures` | Background compaction passes that failed since process start. |
| `disk_free_bytes` | Free bytes at the guardrail's last sample; `null` when the guardrail is off or the probe failed. |
| `disk_blocked` | `true` while ingest is refused because free space is under the floor. |
| `disk_blocked_spans` | Cumulative spans refused by the disk floor. |
| `retention_sweeps` / `retention_blocks_dropped` / `retention_bytes_reclaimed` | Automatic retention outcomes since process start. |
| `cold_blocks` | Committed cold Parquet blocks right now — the file set a full scan opens. |
| `cold_merges` / `cold_blocks_merged` | Cold-to-cold merges completed since process start, and the blocks they consumed. |
| `cold_merge_failures` | Cold-to-cold merge passes that failed since process start. |

For scraping rather than polling, the same numbers are on [`GET /metrics`](#get-metrics)
in Prometheus format.

## GET /v1/meta

Edition/capability handshake, on both the OSS store and the EE `fleet-query` node — same
shape, different values. Each node embeds its own SPA bundle (the OSS binary's is EE-free;
`fleet-query`'s adds the Fleet · EE views) — that's the real edition boundary, decided at
build time. `/v1/meta` isn't a gate; it only drives the console's edition badge.

```bash
curl -s http://localhost:4318/v1/meta
# -> {"edition":"oss","fleet":false,"judge":false,"version":"0.3.0","price_table":"159b5b2f@2026-10-21"}
```

| Field | Meaning |
|---|---|
| `edition` | `"oss"` or `"ee"` — which binary answered. |
| `fleet` | `true` on the EE `fleet-query` node; drives the console's edition badge only — the EE views are already present or absent per which binary's bundle is serving. |
| `judge` | Whether this build has the managed/BYO-key judge path compiled in (`--features judge`). |
| `version` | `CARGO_PKG_VERSION` of the serving binary. |
| `price_table` | The price table deriving `cost_usd` right now: the built-in baseline's `<commit>@<date>`, or `<file's version>+<baseline>` under `--price-table`. Nothing per span records its table; each Parquet block names the one in force when it was written (`evald.price_tables`). |

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

- `spans` — a view over the hot tier ∪ the committed cold Parquet blocks, with
  one row per `(trace_id, span_id)` within the hot tier, within a block, and
  across the two (the hot copy wins). So a span sent twice (a client's resend, an
  exporter's retry of a request evald had already taken) counts once, and a query
  misses no span across an in-flight compaction. Copies flushed from two
  different WAL segments sit in two blocks and count twice until a merge puts
  them in one: an hour partition's merge, its day's, or `evald compact`
  ([FORMAT.md § Merged blocks](./FORMAT.md#merged-blocks)). A day of more than
  `--cold-merge-max-spans` spans is more than one block, so copies can stay
  apart there. Deduping across blocks at query time would hold every cold key in
  memory and keep a filter on `start_unix_nano` from reaching the Parquet scan;
  the `GET` reads above do dedupe across blocks. Columns = the flat Parquet schema
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
(the measured numbers are in the [README's Performance section](../README.md#performance)).

## GET /v1/traces/{trace_id}/scores

**What did this trace score?** For a multi-span agent trace this is not the same question as
[`GET /v1/scores?trace_id=`](#get-v1scores), which returns only scores stated about the
trace *itself* — so a trace whose every span is scored reads as unscored through it.

```bash
curl -s http://localhost:4318/v1/traces/<trace_id>/scores
# -> [{"name":"faithfulness","value":0.5,"measured":false,"function":"mean","n":2,
#      "contributing_span_ids":["bb","dd"],"data_type":"numeric"}]
```

| Field | Meaning |
|---|---|
| `measured` | `true` when a score of this name is attached to the trace itself — authoritative, returned verbatim. `false` when derived from span scores. |
| `function` | The combining function, absent for a measured score. |
| `n` | How many spans contributed. |
| `contributing_span_ids` | The spans behind a derived value, so it is traceable to its evidence. |
| `data_type` | `numeric` or `boolean`. A boolean rollup stays boolean, so `0.0` from `all` reads as "a step failed" rather than "the mean was zero". Contributors of differing types degrade to `numeric` — a set that is not uniformly boolean has no boolean answer. |

Semantics (full statement in `PLAN.md` §2.4):

- **Measured beats derived.** A score on the trace is never overridden by a computed one.
- **A derived value is labelled**, with its `n` and function. A rolled-up number is an
  inference, not a measurement.
- **Absent stays absent.** A name nothing carries is simply not in the list — never `0`.
- **A scored span is authoritative for its subtree.** For
  `root → {retrieve, synthesize → {llm_1, llm_2}}`, a score on `synthesize` represents its
  children rather than being averaged alongside them.
- **The function is per score name and declared** (`--rollup name=fn`): `mean` (default,
  numeric), `all` (default, boolean), `min`, `max`, `sum`, `any`. Categorical and free-text
  scores are skipped — there is no defensible mean of two category labels.

Rollup is computed at read time, so a score attached later is reflected with no rebuild, and
changing `--rollup` changes the answer for traces already stored.

## GET /metrics

Prometheus metrics, [text exposition format][expfmt] 0.0.4
(`Content-Type: text/plain; version=0.0.4; charset=utf-8`).

[expfmt]: https://prometheus.io/docs/instrumenting/exposition_formats/

```bash
curl -s http://localhost:4318/metrics
# -> # HELP evald_spans_ingested_total Spans durably ACK'd since process start, ...
#    # TYPE evald_spans_ingested_total counter
#    evald_spans_ingested_total 4096
#    ...
```

| Series | Type | Meaning |
|---|---|---|
| `evald_build_info{version}` | gauge | Always `1`; the running version rides the label. |
| `evald_spans_ingested_total` | counter | Spans durably ACK'd, counted at the committing fsync. |
| `evald_spans_shed_total` | counter | Spans shed by durable-backlog backpressure (answered `429`). |
| `evald_hot_spans` | gauge | Un-compacted spans resident in the hot tier. |
| `evald_hot_spans_max` | gauge | Configured hot-tier bound (`0` = unbounded). |
| `evald_ingest_shedding` | gauge | `1` while ingest is shedding. |
| `evald_ingest_channel_capacity` | gauge | In-flight appends before a channel-full shed. |
| `evald_compactions_total` | counter | Background compaction passes completed. |
| `evald_compaction_failures_total` | counter | Background compaction passes that failed. |
| `evald_cold_blocks` | gauge | Committed cold Parquet blocks — the file set a full scan opens. Merging bounds it; unbounded growth here is what ends in `EMFILE` on the read path. |
| `evald_cold_merges_total` | counter | Cold-to-cold merges completed. |
| `evald_cold_blocks_merged_total` | counter | Cold blocks consumed by those merges. |
| `evald_cold_merge_failures_total` | counter | Cold-to-cold merge passes that failed. |
| `evald_wal_bytes` | gauge | Bytes currently held in the write-ahead log. |
| `evald_disk_free_bytes` | gauge | Free bytes at the disk guardrail's last sample. **Omitted** when the guardrail is off or the probe failed — a `0` there would read as "disk full" to every alert built on it. |
| `evald_disk_blocked` | gauge | `1` while ingest is refused because free space is under the floor. |
| `evald_spans_disk_blocked_total` | counter | Spans refused by the disk floor (distinct from `evald_spans_shed_total`). |
| `evald_retention_sweeps_total` | counter | Automatic retention sweeps completed. |
| `evald_retention_blocks_dropped_total` | counter | Cold blocks dropped by automatic retention. |
| `evald_retention_bytes_reclaimed_total` | counter | Bytes reclaimed by automatic retention. |
| `evald_eval_events_ingested_total` | counter | `gen_ai.evaluation.result` events (span events or log records) stored as scores. |
| `evald_eval_events_malformed_total` | counter | `gen_ai.evaluation.result` events dropped as malformed (everything else in the same request is unaffected). |
| `evald_log_records_total{outcome}` | counter | OTLP log records received, by what became of them: `span` or `score` (kept), `folded` (recognised, and covered by another record: an accepted tool decision, an assistant response), `unmapped` (not recognised; dropped under `--logs genai`), `off` (dropped under `--logs off`), `malformed` (an evaluation result too malformed to keep). Every outcome is published from start-up, zeros included. Counted when a request is mapped, so a request the store then refuses is counted again when it is retried. |
| `evald_duplicate_calls_dropped_total` | counter | Spans dropped at ingest as a second copy of a call already stored: the same call sent as a log event and a span, or re-sent. |
| `evald_redactions_total{rule}` | counter | Sensitive values rewritten on ingest, one series per rule. Counts **occurrences in the stored representation**, not distinct values — `input.value` is promoted to `input_value` *and* preserved in `raw_attributes`, so one email in a prompt is rewritten in both copies. Absent entirely when redaction is off. |

### LLM usage series

Recorded for every **LLM span** (an OpenInference `LLM` kind, a resolved model, or `gen_ai.request.*` /
`gen_ai.response.*` / `gen_ai.usage.*` attributes on a span that is not an agent, chain or tool) at the
same commit point as `evald_spans_ingested_total`, so they cover exactly the spans that were accepted.
Retriever, tool, chain and agent spans are not counted: an agent span names its model and may report the
usage of the whole run, which its model calls already reported. A restart does not re-count spans recovered from the WAL. Turn them off
with `--no-usage-metrics`.

**Labels** on every per-span series: `gen_ai_provider_name`, `gen_ai_request_model`, `service_name`
(`unknown` when a span carries none). Never user or session. Each is capped; a value past its cap is
folded into `other` and counted in `evald_usage_labels_folded_total` (models: `--metrics-model-cap`,
default 100; providers 16; services 32; and at most 2048 label tuples overall).

| Series | Type | Meaning |
|---|---|---|
| `evald_llm_requests_total` | counter | LLM spans accepted. |
| `evald_llm_request_errors_total` | counter | …of which carried an error status. |
| `evald_llm_cost_usd_total` | counter | Sum of `cost_usd` over spans that carry one. **Partial** while `evald_llm_spans_without_cost_total` is rising. |
| `evald_llm_spans_without_cost_total` | counter | Spans with no `cost_usd`: the cost counter understates spend by what these cost. |
| `evald_llm_input_tokens_total` | counter | Input (prompt) tokens as the span reported them; whether that includes cached tokens depends on the instrumentation. |
| `evald_llm_output_tokens_total` | counter | Output (completion) tokens. |
| `evald_llm_cache_read_tokens_total` | counter | Cache-read input tokens. |
| `evald_llm_cache_write_tokens_total` | counter | Cache-write (creation) input tokens. |
| `evald_llm_reasoning_tokens_total` | counter | Reasoning output tokens. |
| `gen_ai_client_operation_duration_seconds` | histogram | Wall time of the span (`end − start`), seconds. Buckets: `0.01 … 81.92` (the OpenTelemetry GenAI advisory boundaries, doubling from 10 ms) plus `163.84`, `327.68` for long generations and reasoning models. |
| `gen_ai_client_operation_time_to_first_chunk_seconds` | histogram | Time to first chunk, seconds, same buckets. **Only series whose spans carried a time-to-first-token attribute appear**; spans without one are never observed and never estimated. Attributes read: `gen_ai.response.time_to_first_chunk` (s), `ai.response.msToFirstChunk`, `ai.stream.msToFirstChunk`, `time_to_first_token_ms` (ms). |
| `evald_eval_score_mean{evaluator}` | gauge | Mean of the most recent numeric scores per evaluator (window below). Run-level aggregates are excluded. Boolean scores read as a pass rate. |
| `evald_eval_score_window_samples{evaluator}` | gauge | Scores currently in that window (at most 1024). |
| `evald_eval_scores_untracked_total` | counter | Numeric scores ignored because 64 evaluator names were already tracked. Present only once that happens. |
| `evald_usage_series` | gauge | Label tuples currently tracked (bounded at 2048). |
| `evald_usage_labels_folded_total` | counter | Spans whose provider, model, service or tool label was folded into `other`. Non-zero means raise `--metrics-model-cap` or run one evald per fleet segment. |
| `evald_tool_calls_total{gen_ai_tool_name,service_name}` | counter | Tool spans accepted. Tool names are capped at 64 and pairs at 512, past which a call counts under `other`. A call traced at two layers (a framework's tool span and the MCP span inside it) counts at each; the session rollup is the exact count. |
| `evald_tool_call_errors_total{gen_ai_tool_name,service_name}` | counter | …of which carried an error status or an `error.type`. The ratio of the two is the tool-error rate. |

**How to read them.** Counters are *since process start* and **approximate under exporter retries**: an
OTLP exporter that re-sends a batch it never got an ACK for is counted twice, because the store only
de-duplicates at read time. `POST /v1/sql` over the stored spans is the exact figure. The score gauge is a
per-process rolling mean of the last 1024 scores per evaluator, not a historical average. Series names are
a compatibility surface; they will not be renamed within a minor version.

Total spans and total scores are deliberately **not** exposed here: both need a full scan,
and paying for one on every scrape would make the monitoring endpoint the outage. Query
them through [`POST /v1/sql`](#post-v1sql), where the cost is the caller's choice.

Unlike the probes below, `/metrics` **is** subject to the bearer-token gate when it is
armed — a scrape exposes ingest rates and backlog depth. Prometheus supports that with
`authorization:` / `bearer_token_file:` in `scrape_configs`.

## GET /healthz, GET /readyz

Kubernetes probes. **Both are exempt from the bearer-token gate** — a kubelet sends no
`Authorization` header, and a liveness probe that `401`s is a crash loop. Neither reveals
anything a port scan would not.

| Endpoint | Answers | Responses |
|---|---|---|
| `/healthz` | liveness — is this still a working server? | always `200 ok`. It does **not** consult the store: failing liveness on a slow disk or a wedged compactor would have the kubelet kill a process that still holds a good WAL. |
| `/readyz` | readiness — should traffic come here? | `200 ready`, or `503` once the store's writer task is gone and nothing can be made durable. |

Shedding is **not** a readiness failure: `429 + Retry-After` is the documented backpressure
contract, so a node under load stays ready and keeps applying it rather than being pulled
from rotation exactly when that matters.

```bash
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:4318/healthz   # 200
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:4318/readyz    # 200
```

## POST /mcp

A [Model Context Protocol](https://modelcontextprotocol.io) server, so a coding agent can ask
evald about this store: "which session cost the most today?", "why did the eval regress?". The
Streamable HTTP transport, answering each POST with JSON (no event stream, no session state);
protocol revisions `2025-06-18`, `2025-03-26` and `2024-11-05`. Behind the bearer gate like
every route. A request carrying an `Origin` that is not loopback is refused with `403`, so a
web page cannot drive a local evald through the browser, DNS rebinding included (an `Origin`
that matches the request's `Host` is refused too: under rebinding the browser sends the
attacker's name as both). Agents and CLIs send no `Origin`.

```bash
claude mcp add --transport http evald http://127.0.0.1:4318/mcp
# with the gate armed:   --header "Authorization: Bearer $EVALD_AUTH_TOKEN"
```

| Tool | Answers |
|---|---|
| `sql` | Read-only SQL over `spans` and `scores`, as [`POST /v1/sql`](#post-v1sql). |
| `get_trace` | A trace's spans in start order, with its scores. |
| `list_sessions`, `get_session` | The session rollups above. |
| `cost_report` | `evald cost`: tokens and cost by model, user, session, service or provider. |
| `latency_report` | `evald latency`: model-call percentiles by model, provider or service. |
| `compare_runs` | `evald eval compare`: per-evaluator delta between two eval runs, with Welch's t where the runs carry statistics. |
| `failing_items` | A run's lowest per-item scores (below `below`, default 1), each with the span or trace it scored. |
| `add_score` | Write a score onto a span, trace or session. **Only with `serve --mcp-allow-writes`**; otherwise it is not listed and a call is refused. |

Every tool is read-only (`readOnlyHint`) except `add_score`. Results come as JSON text and as
`structuredContent`; strings longer than 2,000 characters (a captured prompt) are cut and say so,
so one trace cannot fill the agent's context. A failing tool returns `isError: true` with the
reason. `GET` and `DELETE /mcp` are `405`. For a store no server holds, `evald mcp --data-dir`
serves the same tools on stdio ([CONFIG.md](./CONFIG.md#evald-mcp)).

| Status | Meaning |
|---|---|
| `200` | The JSON-RPC response (or a batch of them). |
| `202` | The message was a notification or a response: nothing to answer. |
| `400` | The body is not JSON (a JSON-RPC parse error). |
| `403` | A browser `Origin` from another site. |

## Everything else — the embedded SPA

Any `GET` not matched above falls through to the embedded single-page app
(rust-embed; trace list → trace tree → scores, plus a SQL console) with an
`index.html` fallback for client-side routes. It is registered as the router
**fallback**, so it can never shadow a `/v1/*` API route. Works air-gapped —
all assets are compiled into the binary.

The fallback is for a browser: any other method, and any path under `/v1/`, is
`404 evald: no such endpoint: <METHOD> <path>`. So an OTLP exporter pointed at a
signal evald does not take (`POST /v1/metrics`) sees a failure instead of a `200`
it would read as accepted.

```bash
curl -s -o /dev/null -w '%{http_code} %{content_type}\n' http://localhost:4318/
# -> 200 text/html; charset=utf-8
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://localhost:4318/v1/metrics
# -> 404
```
