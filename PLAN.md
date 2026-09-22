# evald — Build Plan (PoC → GA)

> Phased plan for the embedded OTel-native trace + eval store. Concrete per-phase
> deliverables, the MVP cut, build order, architecture, data model, crate stack.
> Honest about what is deferred. Last updated 2026-06-23. Status: Planning (pre-PoC).

## 0. The cut, stated once

evald is **one process, one binary**: an OTLP receiver, a durable WAL, an embedded
hot tier, time-partitioned Parquet queried by DataFusion, an axum API, and an
embedded SPA. The focus we build toward is the **offline dataset/experiment
eval-regression loop with CI exit codes**, not the binary format and not "+evals"
generically.

## 1. Architecture

### 1.1 Data flow

```
   OpenInference / OTel SDKs (LangChain, LlamaIndex, OpenAI, Anthropic,
   Vercel AI SDK, OpenLLMetry) — one env var:
        OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
                          │
   ┌──────────────────────┴───────────────────────┐
   │                  RECEIVER                     │
   │  4318 OTLP/HTTP POST /v1/traces (MVP)         │
   │  4317 OTLP/gRPC TraceService/Export (Beta)    │
   │  x-protobuf | json | gzip                     │
   │  decode → opentelemetry-proto (prost types)   │
   │  OTLP-JSON special-cased: hex traceId/spanId, │
   │     int enums, int64-as-string, lowerCamelCase│
   └──────────────────────┬───────────────────────┘
                          │ NormalizedSpan (dialect: OpenInference vs gen_ai.*)
                          ▼
   ┌───────────────────────────────────────────────┐
   │ bounded tokio::mpsc (backpressure, no SILENT   │
   │ drop) → on full: HTTP 429/503 + Retry-After    │
   │ + load-shed metric. Channel + memtable/L0      │
   │ triggers sized from a MEASURED ingest budget.  │
   └──────────────────────┬───────────────────────┘
                          ▼  single WRITER task
   ┌───────────────────────────────────────────────┐
   │  WAL = THE ACK BOUNDARY                         │
   │  append to durable WAL segment + fsync → ACK    │
   │  BEFORE the hot-tier insert, so ACK latency is  │
   │  decoupled from hot-tier compaction stalls.     │
   └──────────────────────┬───────────────────────┘
                          ▼
   ┌───────────────────────────────────────────────┐
   │  HOT TIER (append-mostly index over recent)     │
   │  keyspaces: spans_recent | scores | meta        │
   │  serves live-tail / last-N-min, non-blocking    │
   │  readers. KV-separation keeps big payloads off   │
   │  the index path. (Engine choice benchmarked —    │
   │  §6, see RISK: hot-tier maturity.)               │
   └──────────────────────┬───────────────────────┘
                          ▼  background compactor (budget-driven, not a fixed 5min/64MB)
   ┌───────────────────────────────────────────────┐
   │  COLD TIER — time-partitioned Parquet           │
   │  blocks/YYYY/MM/DD/HH/*.parquet                 │
   │  + trace_id→partition index in redb             │
   │  retention = drop partition dirs (cheap, local) │
   └──────────────────────┬───────────────────────┘
                          ▼
   ┌───────────────────────────────────────────────┐
   │  QUERY ENGINE — DataFusion (pure Rust)          │
   │  SQL over Parquet + hot keyspace; predicate /   │
   │  projection / bloom pushdown. Dedups by         │
   │  (trace_id, span_id) across the hot/cold        │
   │  boundary so in-flight compaction never shows   │
   │  duplicates or gaps. Never blocks ingest.       │
   └──────────────────────┬───────────────────────┘
                          ▼
   ┌───────────────────────────────────────────────┐
   │  API (axum) /v1/traces (ingest) /v1/spans       │
   │  /v1/datasets /v1/runs /v1/scores               │
   │  /v1/span_annotations (Phoenix-compat, {data:[]})│
   │  /v1/runs/{id}/compare                          │
   └──────────┬───────────────────────┬─────────────┘
              ▼                       ▼
   ┌───────────────────┐   ┌───────────────────────┐
   │ EVAL RUNNER       │   │ embedded SPA (rust-    │
   │ (off hot path)    │   │ embed, SPA fallback)   │
   │ offline: dataset→ │   │ traces · scores ·      │
   │   task→trace→score│   │ run-compare views      │
   │ online: sampled   │   └───────────────────────┘
   │   live spans→score│
   └───────────────────┘
```

### 1.2 Why writes don't block reads, stated correctly

All ingest funnels through **one in-process writer task**. This sidesteps the
**multi-process file lock** problem — but it makes that single writer the throughput
ceiling and the head-of-line blocker, so the **WAL append is the ACK boundary**: we
fsync the WAL and ACK *before* the hot-tier insert, decoupling client-visible latency
from any compaction stall. Ingest storage and query storage are **separate engines**,
so an analytical scan never contends with the OTLP firehose. Live-tail reads the hot
tier via non-blocking readers; eval queries read cold Parquet via DataFusion.

### 1.3 The hot→cold commit protocol (the real unsolved problem, solved explicitly)

Compaction crosses three stores (hot tier, Parquet, redb index); a naive "atomic
directory swap" is a distributed commit inside one process and can double-count or
lose spans on crash. The protocol:

1. Write the Parquet block to a **temp path**, `fsync` the file, `fsync` the parent
   directory.
2. In **one redb transaction**, update the `trace_id→partition` index **and** record
   the new **high-water-mark**.
3. **Only then** truncate the WAL / hot tier up to that watermark.
4. On recovery: replay everything in the hot tier / WAL **above the redb watermark**.
5. The query layer **dedups by `(trace_id, span_id)`** across the hot/cold boundary,
   so an in-flight compaction can never surface duplicates or gaps.

### 1.4 Ingest correctness

- Normalize the two semantic conventions into **one internal span model**, detecting
  dialect by presence of `openinference.span.kind` vs `gen_ai.*`. Keep raw attributes
  for lossless replay.
- Read LLM span kind from the `openinference.span.kind` **attribute**
  (LLM/CHAIN/RETRIEVER/…), **not** the OTel `Span.kind` enum (CLIENT/SERVER/INTERNAL).
- Token normalization maps **both** directions: OI uses `prompt`/`completion`, gen_ai
  uses `input`/`output`; `cache_write` ↔ `gen_ai.usage.cache_creation.input_tokens`
  (name mismatch to watch). gen_ai has **no cost attribute** → derive cost from a
  bundled price table.
- Tolerate both old per-message **events** and new
  `gen_ai.input.messages`/`gen_ai.output.messages` **attribute** form; pin a semconv
  version, be `OTEL_SEMCONV_STABILITY_OPT_IN`-aware, version the mapping.
  (`gen_ai.system` is deprecated → `gen_ai.provider.name`.)
- Large payloads (`input.value`, `gen_ai.input.messages`) are size-capped/truncated;
  KV-separation keeps big blobs off the hot index path.

## 2. Data model

### 2.1 NormalizedSpan (one model for both conventions)
Fields: `trace_id [u8;16]` (hex-decoded from OTLP-JSON, not base64), `span_id [u8;8]`,
`parent_span_id`, `name`, `oi_kind` (LLM|EMBEDDING|CHAIN|RETRIEVER|RERANKER|TOOL|AGENT|
GUARDRAIL|EVALUATOR|PROMPT — from the attribute), `otel_kind`, `start/end_unix_nano`,
`status`, unified `model`/`provider`, `tokens {prompt, completion, total, cache_read,
cache_write, reasoning}`, `cost_usd` (prefer `llm.cost.*`, else token×price table),
`input_messages`/`output_messages`/`retrieval_docs` (JSON), `session_id`, `user_id`,
`service_name`, `scope`, `raw_attributes` (lossless), `events`/`links`.

### 2.2 Score — the universal eval object (Phoenix-compatible)
`Score { id, target: TraceId|SpanId|SessionId|RunId, name, num_value, str_value,
data_type: Numeric|Categorical|Boolean|Text, source: Eval|Human|Api, comment,
config_id, ts }`. Because evald is OTel-native, `span_id`/`trace_id` **is** the join
key: online, offline, and human scores share **one storage path and one schema**.
`ScoreConfig { id, name, data_type, min, max, categories, description, is_archived }`
enforces schema so the CLI/API reject malformed scores.

**`/v1/span_annotations` Phoenix compat (corrected):** the real body is a
`{"data":[ {...} ]}` **batch-array envelope** with an optional per-item `identifier`.
We accept that envelope (a literal per-item-only impl would 422 against Phoenix
clients). Auth header (bearer / api_key) to be verified against the OpenAPI spec
before claiming drop-in.

### 2.3 Dataset / Run (offline eval spine)
`DatasetItem { id, input, expected_output?, metadata }` (JSONL one/line);
`Dataset { id, name, items[], json_schema? }`; `Run { id, dataset_id, task_ref,
evaluator_set[], git_sha/label, created_at }`; `RunItem { run_id, dataset_item_id,
trace_id, span_id }`; `RunAggregate { run_id, metric_name, mean, pass_rate, count }`.
Run comparison diffs `RunAggregate` by `metric_name` across two `run_id`s.

### 2.4 Score rollup — what a *trace* scores when its spans are scored (RESOLVED)

This was an open model gap: a Score attaches to a span, but an agentic task emits a
multi-span trace with no single canonical span to hang the answer on, so "the trace's
faithfulness" was **undefined** — and `scores_for_target(Trace(id))` returned nothing
at all for a trace whose every span was scored. Resolved as follows; implemented in
`src/rollup.rs`, served by `GET /v1/traces/{trace_id}/scores`.

1. **Writing is unchanged, and the question is asked at read time.** A score on a span
   means that span; a score on a trace means the trace. Nothing is promoted on write,
   so no stored score changes meaning and there is nothing to migrate. The rollup runs
   over whatever is stored at the moment it is asked — so a score attached later is
   picked up with no rebuild.
2. **Measured beats derived.** A score attached directly to the trace IS the answer; a
   derived value never overrides what a human or an evaluator stated about the trace.
3. **A derived value says so, and carries its `n`.** It comes back with
   `measured: false`, the function that produced it, the contributor count and the
   contributing span ids. A rolled-up number is an inference, not a measurement; given
   that evald ships Welch's-t gating and judge calibration precisely to avoid
   overstating numbers, rendering a derived score identically to a measured one would
   undercut the whole posture.
4. **Absent stays absent.** A name nothing carries yields *no score* — never `0`, never
   a fabricated pass. Same rule as a Tier-1 evaluator SKIPping a missing field.
5. **A scored span is authoritative for its subtree.** For
   `root → {retrieve, synthesize → {llm_1, llm_2}}`, if `synthesize` is scored *and* its
   children are, averaging all three double-counts: the score on `synthesize` is *about*
   what its children did. The walk therefore takes the **shallowest** carrier of each
   name per branch and does not descend past it — per name, so one trace can roll
   `faithfulness` from one depth and `toxicity` from another.
6. **The combining function is per score name and declared, never inferred** (`--rollup
   name=fn`): `mean | min | max | sum | all | any`. One global function cannot be right
   for every metric — cost wants `sum`, a pass/fail wants `all`, a quality gate usually
   wants `min` because one hallucinating step in a ten-step agent should fail the trace
   and a mean dilutes it. Defaults: `mean` for numeric, `all` for boolean. Categorical
   and free-text scores are skipped — there is no defensible mean of two category
   labels.

**RunItem attach point, per the above:** `RunItem` keeps `trace_id` as the durable
link and `span_id` only where the task produced an unambiguous single span. A run's
per-item score attaches to the **trace**, which makes it measured and therefore
authoritative; the rollup is what answers for traces scored only at the span level.
The 1:1 item↔trace assumption is thereby fine — it is item↔*trace*, never
item↔*span*.

## 3. Crate stack

| Crate | Pin | Role | Why |
|---|---|---|---|
| `tokio` | 1.x | runtime, bounded mpsc | backpressure via `send().await` |
| `axum`+`hyper` | 0.7 / 1.x | OTLP/HTTP 4318 + API | one framework, SPA fallback |
| `tower-http` | 0.6.x | gzip, fallback, middleware | OTLP needs gzip |
| `tonic` | 0.12.x | OTLP/gRPC 4317 (Beta) | TraceService/Export stub |
| `opentelemetry-proto` | 0.32.0 | prost wire types | single source of truth; don't hand-roll |
| `prost` | 0.13.x | decode OTLP/HTTP-protobuf | same decode path |
| `serde`/`serde_json` | 1.x | OTLP-JSON + JSON API | custom hex-id/int-enum/int64-string |
| **hot tier** | none — in-process | WAL + recent-span index | **decided 2026-09-16: no LSM** (`HOT_TIER_DECISION.md`). The tier is a read cache over data the WAL has already made durable, sized by one compaction interval and capped by `--max-hot-spans`; an LSM's spill/recovery/indexing buys nothing at that window and costs a second on-disk format |
| `datafusion`+`arrow`+`parquet` | df 54 / arrow+parquet 58 (df 54 depends on arrow/parquet 58.3 → one Arrow in the tree) | **cold query engine** | fastest single-node Parquet; **pure-Rust, no C++ → clean static musl** |
| `redb` | 4.1.x | trace_id→partition index + watermark | pure-Rust COW B+tree, crash-safe txns; **not a drop-in LSM firehose buffer** |
| `usearch` | latest | optional HNSW (Tier-2 semantic) | **behind a cargo feature; deferred; C++ core needs musl care** |
| `rust-embed` | 8.x | bundle SPA | UI ships in the binary |
| `clap` | 4.x | CLI | headless-first, CI exit codes |
| `reqwest` | 0.12.x | BYO-key judge/embedding | Tier-3, user's tokens |
| `tracing` | 0.1.x | internal logs | self-observability |
| `anyhow`/`thiserror` | 1.x | errors | bin/lib ergonomics |

**Packaging:** `cargo-dist` (installers, Homebrew, `cargo-binstall`), static musl
x86_64 + aarch64 via `cross`/`muslrust`, distroless/chainguard-static image. Release
profile inherits `opt-level="z"`, `lto=true`, `codegen-units=1`, `strip=true`.

**Rejected (corrected rationale):** `duckdb` — **not** for "single-writer" reasons
(our own design serializes ingest through one writer voluntarily; DataFusion is a
query engine and cannot "answer" a write-concurrency concern). Rejected on
**write-pattern grounds**: DuckDB is RAM/analytics-tuned with per-small-write MVCC
snapshot overhead, not tuned for a sustained firehose of tiny appends; plus it links
C++ libduckdb (complicates static musl). `lance`/`lancedb` — heavy dep tree fights
static-musl single-binary.

## 4. MVP v0.1 — minimal, demoable end-to-end

**The one demo:** point an OpenInference SDK at `localhost:4318` → see the trace in
the UI → run one eval from a JSONL dataset → see the score attached to the span.
Single binary, no external services.

### IN (v0.1)
1. OTLP/HTTP receiver on 4318 (protobuf + json + gzip). gRPC 4317 deferred — 4318 is
   the SDK default and covers the common case.
2. OTLP-JSON correct decode (hex ids, int enums, int64-as-string, lowerCamelCase).
3. OpenInference + gen_ai normalization into one span model; dialect auto-detect; raw
   attributes retained.
4. Tiered store — durable WAL + hot index + background compaction to Parquet + redb
   trace_id index + watermark. **Survives kill-9** (commit protocol §1.3).
5. DataFusion query; `GET /v1/spans`, `GET /v1/traces/{id}`, with hot/cold dedup.
6. **Tier-1 native scorers only** (zero-cost, no network): `exact_match`, `contains`,
   `regex`, `json_valid`, `json_schema`, `levenshtein`, `latency`, `cost`. **Fixed
   built-in set only — no user shell/wasm code** (custom-evaluator sandboxing is
   deferred; shipping the constraint explicitly is the minimal answer).
7. Dataset (JSONL) + YAML run config (dataset + task + evaluators[] + thresholds).
8. CLI: `evald serve`, `evald eval run --config eval.yaml` (exit-nonzero on threshold
   fail), `evald eval compare <runA> <runB>`. **This offline regression loop is the
   core of the tool.**
9. Score storage + `/v1/scores` + `/v1/span_annotations` ({data:[]} envelope).
10. Thin embedded SPA — trace list, single-trace tree, scores tab. UI may lag the API;
    headless is fully usable first.

### Build order (each step independently demoable)
1. ✅ **Done** — opentelemetry-proto wire types + `POST /v1/traces` protobuf decode
   (gzip-aware) → log to stdout. Lives in `src/ingest.rs`; `evald serve` runs it; a
   dependency-free `examples/otlp_demo_payload.rs` fixture + router-level integration
   tests cover the decode/response path. OpenInference (`openinference.span.kind`) and
   `gen_ai.*` (model) attributes are already surfaced in the log line.
2. ✅ **Done** — OTLP-JSON path + normalization into the span model. `application/json`
   is decoded via the opentelemetry-proto serde impl (hex ids, int64-as-string
   timestamps, camelCase) with a small `coerce_otlp_json_ints` bridge for the
   `AnyValue.intValue`-as-string gap; both formats flow through one `normalize` pass
   (`src/normalize.rs`) into `NormalizedSpan` (`src/model.rs`) — dialect detection,
   model/provider, tokens mapped both directions (OI `llm.token_count.*` ↔ gen_ai
   `gen_ai.usage.*`, incl. cache-creation→cache_write), cost, I/O capture, and lossless
   `raw_attributes`. Response format mirrors the request.
3. ✅ **Done** — WAL ACK boundary + hot write + read-back via `/v1/spans` (durability
   across a kill-9 restart, proven). `src/store.rs`: ingest funnels through one writer
   task fed by a bounded channel (full → `429 + Retry-After`, never a silent drop); each
   batch is framed (`[len][crc32][json]`), appended and **fsynced before the ACK**; the
   hot tier is an in-memory index; on open the WAL is replayed (torn/corrupt tail
   detected via length+CRC and truncated). Read-back: `GET /v1/spans` (`?trace_id=&limit=`)
   and `GET /v1/traces/{trace_id}`. Verified with a real `kill -9` + restart.
4. ✅ **Done** (DataFusion deferred) — Parquet compactor + commit protocol + redb index +
   hot/cold dedup. `src/store/{cold,index}.rs` + the compactor in `store/mod.rs`: a
   background task flushes sealed WAL segments to **time-partitioned Parquet**
   (`blocks/YYYY/MM/DD/HH`, flat typed columnar schema, Snappy) via the §1.3 commit
   protocol (temp→fsync→rename→fsync-dir → one redb txn recording blocks + advancing the
   **watermark** → only then delete the WAL segment + drop the hot group); queries union
   hot ∪ cold and **dedup by `(trace_id, span_id)`**. Recovery replays segments above the
   watermark and deletes those at/below it; orphan blocks from a crashed flush are swept on
   open. Verified with a real `kill -9` during compaction. **DataFusion (SQL over the same
   Parquet) is deferred to the eval-aggregate steps** — the step-4 read paths don't need
   SQL; blocks are read directly via `arrow`/`parquet` (and are DuckDB/pandas-queryable).
5. ✅ **Done** — Score table + `/v1/scores` + `/v1/span_annotations`. The universal
   [`Score`] object (target = Span | Trace | Session | Run; value num/str; data_type;
   source Eval | Human | Api; `config_id` carried forward) is persisted in its own redb
   db (`scores.redb`, ACID/durable) — `src/store/scores.rs`: `id → JSON` + a
   `target_key → {id}` multimap for point lookup. Endpoints: `POST /v1/scores` (single or
   array, value as number/string/bool), `GET /v1/scores[?span_id=&trace_id=…]`,
   `GET /v1/scores/{id}`, and `POST /v1/span_annotations` — the **Phoenix-compatible
   `{"data":[…]}` envelope** mapped onto span-targeted scores, with the optional
   `identifier` as the upsert key. `ScoreConfig` *enforcement* stays a Beta item.
6. ✅ **Done** — Tier-1 evaluator trait + JSONL dataset + `evald eval run` writing Scores
   keyed to spans. `src/eval.rs`: a fixed, zero-cost `Evaluator` trait (no user
   shell/wasm) with `exact_match`, `contains`, `regex`, `json_valid`, `levenshtein`; a
   JSONL `DatasetItem` loader and a YAML `EvalConfig` (dataset + evaluators[] +
   thresholds). `evald eval run --config eval.yaml` evaluates each item, persists a
   per-item Score on each item's span/trace (else the run) plus a per-evaluator aggregate
   Score targeting the run, prints a report, and **exits non-zero when an evaluator's mean
   drops below its threshold** (the CI gate). Demo fixtures in `examples/eval/`.
   Deferred: `json_schema` (validator dep), span-derived `latency`/`cost`, Tier-3 judge.
7. ✅ **Done** — `evald eval compare <runA> <runB>` + RunAggregate. Reads back the
   run-targeted aggregate Scores step 6 persisted (ids `{run_id}:agg:{name}`, target
   `ScoreTarget::Run`), diffs their means by evaluator name (`delta = B - A`), and prints
   a table (run_a / run_b / delta / change). `--fail-on-regression` makes it a CI gate —
   **exits non-zero when any shared evaluator drops more than `--tolerance` (default 0)**;
   evaluators present on only one side are surfaced (`new`/`gone`) but never gate. Unknown
   run ids error out. `src/eval.rs`: `compare_runs` / `compare_command`.
8. ✅ **Done** — DataFusion SQL engine + embedded rust-embed SPA. `src/sql.rs`: a
   per-query DataFusion `SessionContext` registers `cold_spans` (a **`ListingTable` over
   the Parquet blocks** — real predicate/projection pushdown; an empty `MemTable` with the
   block schema when there are no blocks yet), `hot_spans` (the un-compacted hot tier as a
   `MemTable`), and `scores` (the redb store materialized), then defines `spans` as a
   **deduped `hot ∪ cold` view** (by `(trace_id, span_id)`, matching the read API). Only
   read statements (`SELECT`/`WITH`/`EXPLAIN`) are accepted — the blocks-write path stays
   the commit protocol, not SQL. Surfaced as `POST /v1/sql` (`{columns,rows,row_count,
   truncated}`) and `evald query "<SQL>"`. `src/ui.rs` + `frontend/`: a dependency-free
   vanilla-JS SPA (trace list → waterfall span tree → span detail + scores, plus a SQL
   console) embedded via **rust-embed** and served as the axum **fallback** (so it never
   shadows `/v1/*`); works air-gapped, no Node toolchain. DataFusion/arrow/parquet are all
   pinned in lockstep at 58 → one Arrow in the tree, no C++ → clean static musl.

### OUT of v0.1 (deferred, with landing phase)
- OTLP/**gRPC** 4317 → v0.2.
- **Tier-3 LLM-as-judge** + BYO-key judges → v0.2.
- **Tier-2 semantic** `similar`/embedding + usearch HNSW → v0.3 (feature flag).
- **Online/live** sampled eval → v0.3.
- Prompt management, playground, dashboards, alerting — **never** in core.
- OTLP metrics/logs ingest — out of scope (separate proto + convention).

## 5. Phases

### PoC — v0.1.0-alpha (internal)
Prove ingest→WAL→hot→query→score in one process: OTLP/HTTP 4318; OTLP-JSON decode;
normalization (OI + gen_ai); WAL ACK boundary + hot store + read-back; first Tier-1
evaluator (`exact_match`) writing a Score keyed to a span; `evald serve` +
`evald eval run` headless. No UI, no compaction yet.

### MVP — v0.1.0 (first public OSS release → lucheeseng827/evald)
All 10 IN-scope items, durable, thin UI: Parquet compaction + commit protocol +
DataFusion + redb (kill-9-durable); full Tier-1 set; JSONL datasets + YAML config;
`eval run`/`eval compare` with CI exit codes; `/v1/span_annotations` Phoenix compat;
embedded SPA; static musl x86_64+aarch64 via cargo-dist + Homebrew + binstall +
distroless; Apache-2.0 LICENSE + NOTICE + README/PLAN/CHANGELOG.

### Beta — v0.5.0
OTLP/gRPC 4317; **Tier-3 LLM-as-judge** as declarative `JudgeSpec {prompt_template,
output_rail, model, provider}` with built-in rails (faithfulness, answer_relevance,
context_precision/recall, hallucination, qa_correctness, custom G-Eval) — **BYO-key,
free**; judge-result caching keyed by (input,output,judge_version); CLI cost-estimate;
**judge calibration vs the user's own human labels** (`eval calibrate`: bias/MAE/RMSE/Pearson,
paired-t CI on the bias, affine bias-correction, divergence "recalibrate" gate — offline, OSS);
**online/sampled** eval off the ingest stream; Tier-2 `similar` + usearch behind a
feature flag; ScoreConfig enforcement; ✅ **score-rollup-up-the-tree semantics resolved
and implemented** (§2.4); richer SPA (run-compare diff, annotation queue). RAGAS-style
rail templates are version-sensitive — pin which versions' semantics we mirror.

User-signal additions (from a 2026-07 sweep of public issue trackers and
practitioner threads):

- **CI-native gate output:** `eval run` / `eval compare` / `suite run` gain
  `--output junit[:path]` (JUnit XML, so gate results render in the native
  test-report views of GitHub/GitLab/Azure/Bitbucket CI) and a stable
  machine-readable JSON report. Exit codes stay the source of truth.
- **Judge scores are advisory by default:** LLM-judge results inform, but only
  deterministic Tier-1 evaluators hard-fail a build unless the config opts a judge
  into gating (`gate: true`) — a non-deterministic score should not flake CI.
- **Legacy GenAI attribute normalization:** accept the deprecated indexed
  `gen_ai.prompt.{i}.*` / `gen_ai.completion.{i}.*` span-attribute shape (still
  emitted by widely deployed instrumentation such as OpenLLMetry) and normalize it
  into the same `NormalizedSpan` messages as the structured
  `gen_ai.input.messages` / `gen_ai.output.messages` shape; the pinned semconv
  mapping carries both for as long as deployed SDKs emit them.
- **OTLP/HTTP conformance tests:** lock in spec behavior the receiver already has
  — the response `Content-Type` mirrors the request's (protobuf in → protobuf
  out), correct OTLP error shapes, `Retry-After` on shed — as an explicit test
  suite, so ingest keeps working with every language's stock exporter.
- **Queryability guarantee, tested + documented:** every span attribute —
  including ones evald doesn't map to a column — is queryable through `/v1/sql`
  via `raw_attributes`, with no renaming or vendor prefix required; plus a docs
  recipe for building a custom domain-specific viewer over the open Parquet
  blocks (DuckDB / pandas / notebook).

### GA — v1.0.0
**OSS core:** frozen on-disk Parquet/WAL format + migration tooling ✅ **2026-09-16**
(`docs/FORMAT.md`, `evald migrate`, and a committed format-1 data-dir the test suite reads
every run); cold-to-cold compaction so the block count — and the open files a scan needs —
stays bounded ✅ **2026-09-16** (`evald compact`, `tests/fd_ceiling.rs`); retention by
partition-drop (local, free, **no artificial local cap**); stable HTTP/CLI API;
semconv-version-pinned mapping with forward-compat tests. **GA gated behind a
sustained-ingest + kill-9-crash-recovery + compaction-under-load soak test with
fsync-correctness verification** ✅ **2026-09-14** (`tests/soak.rs`, `docs/SOAK.md` — the
single biggest storage risk, §6).

## 6. Storage risks the design owns (not the libs')

- **Single writer is the throughput ceiling**, not an escape hatch. It removes the
  multi-process file lock only. Mitigate with the WAL-ACK-boundary so client latency
  is decoupled from compaction.
- **LSM write stalls land on the ACK path if the WAL is not the ACK boundary.** An
  LSM (L0→L1 compaction backing up) stalls writes; serialized ingest would propagate
  that to the client. We ACK at the WAL and size channel/memtable triggers from a
  **measured** ingest budget, with a **load-shed metric + SLO** because shedding will
  happen.
- **Hot-tier maturity is High, not Med.** A 6-month-old new-on-disk-format LSM on the
  crash-durable hot path is the single biggest storage risk. ✅ **Resolved 2026-09-16 by
  not taking it** — `HOT_TIER_DECISION.md`. The shipped segmented WAL plus in-memory index
  stays: the hot tier is a read cache over spans the WAL has already made durable, its size
  is one compaction interval of ingest, and `--max-hot-spans` caps it by shedding rather
  than growing. Measured at that bound — 1.9 KiB resident per 1 KiB span, 4.5 s to replay
  1M spans, 40 ms for a trace lookup across them — every cost an LSM would remove is
  negligible at a realistic window, while an LSM would add a second on-disk format and put
  compaction stalls back on the ACK path. `redb` was never a free swap either (B+tree,
  different write-amp under high small-write ingest). The one real consequence is a sizing
  one, now in `docs/CONFIG.md`: the old 1M default implied ~1.8 GiB resident, so the
  default bound is now 300,000 — ~852 MiB peak for the whole process, against the 1 GiB
  limit the manifests set.
- **Compaction atomicity is High, not Med** — see the explicit commit protocol §1.3.
- **"Never drops under load" is false and removed.** The honest claim is
  durability-of-accepted-spans + explicit backpressure-and-shed (429/503), never
  silent loss.
