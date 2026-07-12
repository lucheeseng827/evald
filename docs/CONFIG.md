# evald configuration reference

Every knob evald reads, in one place. evald is configured by **CLI flags with
`EVALD_*` environment-variable fallbacks** (flag wins over env, env wins over the
default) plus **one YAML file per eval run** (`evald eval run --config`). There is
no config file for the server. Source of truth: the clap derives in
[`src/main.rs`](../src/main.rs) and the serde structs in
[`src/eval.rs`](../src/eval.rs) / [`src/judge.rs`](../src/judge.rs); regenerate
this file when they change.

## `evald serve`

Runs the OTLP/HTTP receiver + durable store + query/SQL API + embedded SPA.

| Flag | Env var | Type | Default | What it does / when to change it |
|---|---|---|---|---|
| `--otlp-http` | `EVALD_OTLP_HTTP_ADDR` | `host:port` | `127.0.0.1:4318` | Bind address for the whole HTTP surface (OTLP ingest, `/v1/*` API, SPA). Loopback by default — it never listens on the network unasked; binding wider is a deliberate act (see [OPERATIONS.md § Security posture](./OPERATIONS.md#security-posture)). |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Directory for all durable state: `wal/`, `blocks/`, `index.redb`, `scores.redb` (layout in [OPERATIONS.md](./OPERATIONS.md#what-the-state-is----data-dir-layout)). Created if absent. |
| `--seal-threshold` | `EVALD_SEAL_THRESHOLD` | int (spans) | `50000` | Seal the active WAL segment after this many spans; sealed segments become eligible for compaction to Parquet. Lower it to get smaller, more frequent Parquet blocks (and a smaller WAL replay on restart); raise it for fewer, larger blocks. |
| `--compact-interval-secs` | `EVALD_COMPACT_INTERVAL_SECS` | int (seconds) | `5` | Background compaction interval. `0` disables background compaction entirely — spans then stay in the WAL + hot tier (reads still see them; the WAL is never truncated). |

Fixed (not flag-exposed) server limits, from [`src/ingest.rs`](../src/ingest.rs)
and `StoreConfig::default()` in [`src/store/mod.rs`](../src/store/mod.rs):

| Constant | Value | Effect |
|---|---|---|
| request body cap | 16 MiB **decompressed** | `POST /v1/traces` bodies larger than this → `413` (the cap is applied after gzip inflation, so a decompression bomb is stopped). |
| ingest channel depth | 1024 batches | When the bounded ingest channel is full, requests are shed with `429 + Retry-After: 1` — never a silent drop. |
| `GET /v1/spans` / `GET /v1/scores` limit | default 100, max 10 000 | `?limit=` is clamped into `[1, 10000]`. |
| `POST /v1/sql` row cap | default 1 000, max 100 000 | `limit` in the request body is clamped; the *result* is capped (`truncated: true`), not the scan. |

## `evald eval run`

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--config` | — | path | `eval.yaml` | The eval YAML (below). |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Where per-item + aggregate Scores are persisted (and the judge cache lives). Must **not** be a data-dir a live `evald serve` holds open — the redb lock is exclusive (see [OPERATIONS.md](./OPERATIONS.md#troubleshooting)). |
| `--estimate` | — | flag | off | Preview the judge token usage + indicative cost for this config **without any network call**, then exit. Tier-1 evaluators are zero-cost, so with no `judges:` it just says so. |

Exit code: `0` when every configured threshold is met, `1` on a threshold
regression — the CI gate.

## `evald eval compare <run_a> <run_b>`

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Where the two runs' aggregate Scores are read from. |
| `--fail-on-regression` | — | flag | off | Exit non-zero when any shared evaluator dropped (B below A) beyond `--tolerance`. |
| `--tolerance` | — | float ≥ 0 | `0.0` | How far an evaluator may drop before it counts as a regression. Negative / non-finite values are rejected at parse time. |
| `--significance` | — | flag | off | Gate only on *statistically significant* regressions: a drop fails the build only when Welch's t shows the `(1 − alpha)` CI on the delta excludes 0. A regression that **cannot** be tested (a run persisted without stats, or n < 2) still gates — it is not forgiven. |
| `--alpha` | — | float in (0, 1) | `0.05` | Significance level for `--significance` and the printed CI (`0.05` → 95 % CI). |

Exit code: `0`, or `1` when `--fail-on-regression` is set and the (raw or
significance-mode) gate trips.

## `evald eval calibrate`

Pairs an LLM-judge's scores with **human** annotations on the same span/trace and
reports bias, MAE/RMSE, Pearson r, a CI on the bias, and the bias-correcting
affine map. Fully offline — reads only the score store.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--judge` | — | string | *required* | Judge score name to calibrate (e.g. `judge_g_eval`). |
| `--human` | — | string | any human score | Only pair against human annotations with this name. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Score store to read. |
| `--alpha` | — | float in (0, 1) | `0.05` | Significance level for the CI on the bias. |
| `--threshold` | — | float ≥ 0 | `0.2` | Max drift (MAE, or a significant bias) before the judge is flagged for recalibration. |
| `--fail-on-divergence` | — | flag | off | Exit non-zero when the judge needs recalibration — a CI gate against judge drift. |

## `evald cost`

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--by` | — | `model` \| `user` \| `session` \| `service` \| `provider` | `model` | Attribution dimension (case-insensitive). Untagged spans surface as `(untagged)`. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The spans to report over. |
| `--limit` | — | int | `100` | Max attribution rows printed. |

## `evald query <sql>`

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| *(positional)* `sql` | — | string | *required* | Read-only SQL over the `spans` (hot ∪ cold, deduped) and `scores` tables — the same guard as `POST /v1/sql` (see [API.md](./API.md#post-v1sql)). |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The blocks + score store to query. Takes the redb lock — cannot run against a live `serve` on the same dir. |
| `--limit` | — | int | `1000` | Max rows printed (a truncation note goes to stderr so stdout stays pipeable JSON). |

## Eval YAML (`evald eval run --config`)

Source of truth: `EvalConfig` / `EvaluatorSpec` in [`src/eval.rs`](../src/eval.rs)
and `JudgeSpec` in [`src/judge.rs`](../src/judge.rs). A runnable example is
[`examples/eval/eval.yaml`](../examples/eval/eval.yaml); a judge example is
[`examples/eval/judge.yaml`](../examples/eval/judge.yaml).

### Top level

| Key | Type | Default | What it does |
|---|---|---|---|
| `name` | string | unset | Display name printed in the report. |
| `dataset` | path | *required* | JSONL dataset; relative paths resolve **against the config file's directory**. One `DatasetItem` per line: `output` (required), plus optional `id`, `input`, `expected_output`, `span_id`, `trace_id`, `context` (string array, for RAG rails), `metadata` (object — `latency_ms` / `duration_ns` / `cost_usd` feed the span-derived gates). |
| `evaluators` | list | `[]` | Tier-1 deterministic evaluators (below). Each **kind may appear at most once** — scores and thresholds are keyed by evaluator name, so duplicates are rejected up front. |
| `judges` | list | `[]` | Tier-3 LLM-as-judge entries (below). Requires a binary built with `--features judge` to actually call a provider. |
| `thresholds` | map name → float | `{}` | Per-evaluator minimum **mean** score for the run to pass (the CI gate). Keys are evaluator names (`exact_match`, `judge_g_eval`, …). |

### `evaluators:` entries (Tier-1, deterministic, no network)

Each entry is `{ type: <snake_case name>, ...params }`. The set is fixed — no
user shell/wasm code. An evaluator that lacks what it needs (e.g. no
`expected_output`) **skips** the item rather than failing it; a fully-skipped
evaluator never trips a threshold.

| `type` | Params | Scores 1.0 when… |
|---|---|---|
| `exact_match` | — | `output == expected_output` (trimmed). |
| `contains` | `substring` (optional; else `expected_output` is the needle) | `output` contains the needle. |
| `contains_all` | `substrings` (non-empty list) | every substring present; the value is the fraction present (partial credit), pass = all. |
| `contains_any` | `substrings` (non-empty list) | at least one substring present. |
| `regex` | `pattern` | `output` matches (invalid patterns are rejected at load). |
| `json_valid` | — | `output` parses as JSON. |
| `json_schema` | `schema` (inline JSON Schema) | `output` parses as JSON **and** validates; non-JSON output is a FAIL, not a skip. Offline — external `$ref`s are not fetched. |
| `non_empty` | — | `output` is non-empty after trimming. |
| `length_bounds` | `min` / `max` (at least one) | char length within `[min, max]`. |
| `levenshtein` | `threshold` (default `0.8`) | value = normalized similarity in 0..1; pass when ≥ threshold. |
| `numeric_tolerance` | `tolerance` (≥ 0, default `0`) | both sides parse as numbers and differ ≤ tolerance; skipped when non-numeric. |
| `equals_numeric` | — | both sides parse as numbers and are numerically equal (`"1.0" == "1"`); skipped when non-numeric. |
| `latency` | `max_ms` | item's `metadata.latency_ms` (or `duration_ns`/1e6) ≤ max; skipped when absent. |
| `cost` | `max_usd` | item's `metadata.cost_usd` ≤ max; skipped when absent. |

### `judges:` entries (Tier-3, BYO-key, `--features judge`)

| Key | Type | Default | What it does |
|---|---|---|---|
| `rail` | `g_eval` \| `qa_correctness` \| `answer_relevancy` \| `faithfulness` \| `hallucination` \| `context_precision` \| `context_recall` \| `toxicity` \| `bias` | *required* | The grading rubric. All rails score higher-is-better in `[0, 1]`. |
| `provider` | `anthropic` \| `openai` | *required* | Which provider API to call. |
| `model` | string | *required* | Provider model id. |
| `criteria` | string | unset | **Required by `g_eval`** — the natural-language grading criteria. |
| `name` | string | `judge_<rail>` | Override the evaluator name (also the `thresholds:` key) so two judges can share a rail. |
| `pass_threshold` | float in [0, 1] | `0.5` | Per-item pass cutoff (score ≥ this counts as a pass). |

Judge environment variables (read **at call time only**, never logged or
persisted — see [`src/judge.rs`](../src/judge.rs)):

| Env var | Used when | What it does |
|---|---|---|
| `ANTHROPIC_API_KEY` | `provider: anthropic` | BYO key. Required for real calls (not for `--estimate`). |
| `OPENAI_API_KEY` | `provider: openai` | BYO key. |
| `ANTHROPIC_BASE_URL` | `provider: anthropic` | Override the API base (default `https://api.anthropic.com`) — for gateways/regulated egress. |
| `OPENAI_BASE_URL` | `provider: openai` | Override the API base (default `https://api.openai.com`). |

Judge results are cached in `<data-dir>/judge_cache.redb` keyed by
`(version, provider, rail, model, criteria, item)` — re-running an unchanged eval
is free; the cache stores only scores + reasoning, never inputs or keys.

## Data-format & compatibility

- **WAL** (`wal/<seqno>.wal`): CRC-framed records, `[u32 len][u32 crc32][JSON payload]`.
  **Provisional until the GA freeze** ([`src/store/wal.rs`](../src/store/wal.rs)) — do
  not build tooling against it.
- **Cold blocks** (`blocks/YYYY/MM/DD/HH/*.parquet`): plain Snappy-compressed
  Parquet with a flat, typed columnar schema
  ([`src/store/cold.rs`](../src/store/cold.rs) `schema()` — `trace_id`, `span_id`,
  `model`, token counts, `cost_usd`, `raw_attributes` as a JSON string column, …).
  Deliberately an **open format**: DuckDB / pandas / external DataFusion read the
  files directly off disk.
- **Indexes / scores** (`index.redb`, `scores.redb`, `judge_cache.redb`): redb
  databases, JSON-serialized values. Internal — access them through evald.
- There is **no `format_version` stamp yet and no upgrade/downgrade promise at
  PoC**: a new evald version is expected to read a PoC data-dir, but the safe
  upgrade path is snapshot-then-upgrade (see
  [OPERATIONS.md § Upgrade](./OPERATIONS.md#upgrade)). The Parquet blocks are the
  durable, portable representation — they remain readable by external tools
  regardless.
