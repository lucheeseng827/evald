# evald configuration reference

Every knob evald reads, in one place. evald is configured by **CLI flags with
`EVALD_*` environment-variable fallbacks** (flag wins over env, env wins over the
default) plus **one YAML file per eval run** (`evald eval run --config`). There is
no config file for the server. Source of truth: the clap derives in
[`src/main.rs`](../src/main.rs) and the serde structs in
[`src/eval.rs`](../src/eval.rs) / [`src/judge.rs`](../src/judge.rs); regenerate
this file when they change.

## Logging — `RUST_LOG`

The one environment variable that is not `EVALD_*`, because it is the standard Rust
one. It applies to **every** command, takes the full
[`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
syntax, and writes to **stderr** so stdout stays parseable.

| `RUST_LOG` | What `evald serve` logs on the ingest path |
|---|---|
| *(unset)* → `info` | **The default.** One line per `POST /v1/traces` (`resource_spans`, `spans`), plus startup, compaction, recovery and guardrail lines. |
| `evald=debug` | Adds the **per-span** detail line — dialect, trace/span id, name, `oi_kind`, model, provider, token counts, `cost_usd`, duration, attribute count. For inspecting what an SDK actually sends. |
| `evald::ingest=debug` | The same per-span line, without turning up the rest of the crate. |
| `warn` | Quietest useful setting. What the throughput numbers in [BENCHMARKS.md](./BENCHMARKS.md) assume. |

**The per-span line is `debug` on purpose, and it is not free to turn on.** Formatting
and colouring its ~14 fields measured **~38,000 instructions per span, about a third of
the ingest hot path**; end to end on a fsync-bound 4-vCPU VM it still cost **+14% of the
server's CPU per span and −7% throughput**. It is off by default and does not belong on
a loaded server. Turning the default *down* to `warn` buys nothing — the two measured
within 0.5% of each other. See [OPERATIONS.md § Logs](./OPERATIONS.md#logs).

## `evald serve`

Runs the OTLP/HTTP receiver + durable store + query/SQL API + embedded SPA.

| Flag | Env var | Type | Default | What it does / when to change it |
|---|---|---|---|---|
| `--otlp-http` | `EVALD_OTLP_HTTP_ADDR` | `host:port` | `127.0.0.1:4318` | Bind address for the whole HTTP surface (OTLP ingest, `/v1/*` API, SPA). Loopback by default — it never listens on the network unasked; binding wider is a deliberate act (see [OPERATIONS.md § Security posture](./OPERATIONS.md#security-posture)). |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Directory for all durable state: `wal/`, `blocks/`, `index.redb`, `scores.redb` (layout in [OPERATIONS.md](./OPERATIONS.md#what-the-state-is----data-dir-layout)). Created if absent. |
| `--seal-threshold` | `EVALD_SEAL_THRESHOLD` | int (spans) | `50000` | Seal the active WAL segment after this many spans; sealed segments become eligible for compaction to Parquet. Lower it to get smaller, more frequent Parquet blocks (and a smaller WAL replay on restart); raise it for fewer, larger blocks. |
| `--compact-interval-secs` | `EVALD_COMPACT_INTERVAL_SECS` | int (seconds) | `5` | Background compaction interval. `0` disables background compaction entirely — spans then stay in the WAL + hot tier (reads still see them; the WAL is never truncated). |
| `--cold-merge-threshold` | `EVALD_COLD_MERGE_THRESHOLD` | int (blocks) | `16` | Merge an hour partition's cold blocks once this many of a similar size have accumulated. Every sealed segment becomes a block, so without merging the block count only grows — and the file set a query opens with it. `0` turns merging off on the compactor's tick; `evald compact` still merges on demand. |
| `--cold-merge-max-spans` | `EVALD_COLD_MERGE_MAX_SPANS` | int (spans) | `1000000` | A merged block never exceeds this many spans; a partition with more becomes several blocks. Keeps retention (which drops blocks whole) and merge memory granular. |
| `--cold-merge-days` | `EVALD_COLD_MERGE_DAYS` | bool | `true` | Also collapse each fully-past UTC day into day blocks, so a low-volume store's block count grows per day rather than per hour. |
| `--cold-merge-day-quiet-secs` | `EVALD_COLD_MERGE_DAY_QUIET_SECS` | int (seconds) | `3600` | A closed day collapses only once nothing has been written into it for this long, so a backfill (or a client with a skewed clock) does not have the whole day rewritten on every tick. |
| `--cold-merge-grace-secs` | `EVALD_COLD_MERGE_GRACE_SECS` | int (seconds) | `60` | How long a merge input stays on disk after it leaves the index, so a query that listed it moments before the merge can still read it. Raise it above your slowest query if a long scan ever races a merge. |
| `--max-hot-spans` | `EVALD_MAX_HOT_SPANS` | int (spans) | `300000` | Durable-backlog bound: shed ingest (`429 + Retry-After`) once this many un-compacted spans are resident in memory. Everything accepted is already durable in the WAL, so this bounds **memory**, not durability — it stops a lagging compactor from OOMing the process. `0` disables the bound. **Size it against your container limit** — see below. |
| `--auth-token` | `EVALD_AUTH_TOKEN` | string (repeatable) | *(none)* | Require `Authorization: Bearer <token>` on **every** request — HTTP (OTLP ingest, `/v1/*`, SPA; `401` on failure) and OTLP/gRPC (`authorization` metadata; `UNAUTHENTICATED` on failure). Each flag occurrence is **one whole token** (a comma is part of the token); repeat it for several accepted tokens (rotation / per-client revocation). `EVALD_AUTH_TOKEN` is a **comma-separated** list, **unioned** with the flag(s) and the file — not overridden. Tokens must be **≥16 printable-ASCII chars** (rejected at boot). **Nothing set — or a present-but-empty value — ⇒ auth OFF** (the default local posture). Arm it before exposing evald on a shared/public network (see [OPERATIONS.md § Security posture](./OPERATIONS.md#security-posture)). Shared-secret gate, **not** TLS. |
| `--rollup` | `EVALD_ROLLUP` | `name=fn` (repeatable, comma-separated) | *(defaults)* | How a score name combines when [`GET /v1/traces/{id}/scores`](./API.md#get-v1tracestrace_idscores) rolls span scores into a trace-level value: `mean` \| `min` \| `max` \| `sum` \| `all` \| `any`. Defaults: `mean` for numeric, `all` for boolean. **A CI gate usually wants `min`** — one bad step in a ten-step agent should fail the trace, and a mean dilutes it. Read-time only: nothing is rewritten, and changing this changes the answer for traces already stored. |
| `--price-table` | `EVALD_PRICE_TABLE` | path | *(built-in table)* | A model price table (the LiteLLM `model_prices_and_context_window.json` shape, or any subset of it) laid over the built-in one; it wins for every model it names. Used at ingest to fill `cost_usd` for spans that carry a model and token counts but no cost of their own. Re-read when the file's modification time changes (a bad edit keeps the previous table); a file that does not parse stops `serve` at start-up. evald makes no network call to update it. Logs a warning when the table is older than 90 days. |
| `--no-usage-metrics` | `EVALD_NO_USAGE_METRICS` | flag | off | Do not record the LLM usage series (cost, tokens, latency, rolling evaluator scores) on `/metrics`. They are on by default: one lock per committed batch and a bounded number of series. evald's own health series are unaffected. See [API.md § LLM usage series](./API.md#llm-usage-series). |
| `--metrics-model-cap` | `EVALD_METRICS_MODEL_CAP` | int | `100` | Distinct model names carried as the `gen_ai_request_model` label before the rest fold into `other` (counted by `evald_usage_labels_folded_total`). Bounds the series count, and so the scrape size (~4–5 KB per series) and the scraper's memory. Providers (16) and services (32) have fixed caps, and there are at most 2048 label tuples. |
| `--redact` | `EVALD_REDACT` | classes, comma-separated, or `all` (repeatable) | *(none)* | **Redact sensitive values before anything is written.** Classes: `email`, `credit_card` (Luhn-checked), `ssn`, `phone`, `ip`, `api_key`, `jwt`. Applies to prompts (`input_value`), completions (`output_value`) and every string in `raw_attributes` (recursing into nested JSON). **Unset ⇒ no redaction** — the rewrite is irreversible, since the raw value never reaches the WAL, a blob or a Parquet block, so it is always an explicit choice. Not scanned: `user_id`, `session_id`, `service_name`, span name — see [OPERATIONS.md § Redaction](./OPERATIONS.md#redaction). |
| `--redact-action` | `EVALD_REDACT_ACTION` | `redact` \| `hash` \| `drop` | `redact` | `redact` → `[REDACTED:<class>]`. `hash` → `[<class>:<16 hex>]`: unrecoverable, but **equal values hash equally**, so "how many distinct users" stays answerable without the store holding the value. `drop` removes the match entirely. |
| `--redact-custom` | — | `name=regex` (repeatable) | *(none)* | An extra rule, e.g. `employee_id=EMP-[0-9]{6}`. Compiled at startup, so a bad pattern fails the process rather than silently never matching under load. |
| `--retention` | `EVALD_RETENTION` | window (`30d`/`72h`/`90m`/`3600s`) | *(none)* | Automatically drop cold Parquet blocks whose spans **all** predate `now − window`, on a timer. **Unset ⇒ no automatic deletion** — evald never removes data unless asked. A block is dropped whole (index entry first, then `unlink`), so reclamation is `O(unlink)` with no row-by-row delete and no vacuum. `evald retention --dry-run` previews the same sweep. |
| `--retention-interval-secs` | `EVALD_RETENTION_INTERVAL_SECS` | int (seconds) | `3600` | How often the automatic sweep runs. Ignored when `--retention` is unset. |
| `--disk-min-free` | `EVALD_DISK_MIN_FREE` | bytes (`512MiB`, `2g`, or a count) | `256MiB` | **Disk floor.** While the data-dir filesystem has less than this free, ingest is refused with `503 + Retry-After: 30` — a clean refusal *before* a write hits `ENOSPC` mid-segment. `0` disables the floor. Measured as space available to an unprivileged process (`statvfs` `f_bavail`), so the root reserve is not counted as usable. |
| `--disk-warn-free` | `EVALD_DISK_WARN_FREE` | bytes | `1GiB` | Log a warning once free space falls below this, while still accepting ingest — the signal before the floor. `0` disables. Must be **greater than** `--disk-min-free`, or `serve` refuses to start (a warning that can never fire before the floor is worse than none). |
| `--disk-check-interval-secs` | `EVALD_DISK_CHECK_INTERVAL_SECS` | int (seconds) | `10` | How often free space is sampled. `0` disables the guardrail entirely — no probe, no floor. Sampling is on a timer, not per request, so an append pays one relaxed atomic read. The guardrail **fails open**: if free space cannot be read (a non-Unix target, or a failing probe) it never blocks ingest, and says so once in the log. |
| `--auth-token-file` | `EVALD_AUTH_TOKEN_FILE` | path | *(none)* | A file of bearer tokens — one per line; blank lines and `#` comments ignored — unioned with any `--auth-token` values. Keeps secrets out of argv/env; rotate by editing the file. |

Fixed (not flag-exposed) server limits, from [`src/ingest.rs`](../src/ingest.rs)
and `StoreConfig::default()` in [`src/store/mod.rs`](../src/store/mod.rs):

| Constant | Value | Effect |
|---|---|---|
| request body cap | 16 MiB **decompressed** | `POST /v1/traces` bodies larger than this → `413` (the cap is applied after gzip inflation, so a decompression bomb is stopped). |
| ingest channel depth | 1024 batches | When the bounded ingest channel is full, requests are shed with `429 + Retry-After: 1` — never a silent drop. |
| `GET /v1/spans` / `GET /v1/scores` limit | default 100, max 10 000 | `?limit=` is clamped into `[1, 10000]`. |
| `POST /v1/sql` row cap | default 1 000, max 100 000 | `limit` in the request body is clamped; the *result* is capped (`truncated: true`), not the scan. |

### Sizing the hot tier

Un-compacted spans live in memory until the compactor writes them to Parquet, so the hot
tier's size is **ingest rate × `--compact-interval-secs`**, capped by `--max-hot-spans`.
Measured at roughly **1.9 KiB resident per span** for an LLM span with a 1 KiB
prompt/completion payload (`HOT_TIER_DECISION.md` §3 — reproduce with
`cargo test --release --test hot_tier_bounds -- --ignored --nocapture`; scale it by your own
payload size, which dominates):

| sustained ingest | hot tier after one 5 s interval | resident |
|---|---|---|
| 1,000 spans/s | 5,000 spans | ~10 MiB |
| 10,000 spans/s | 50,000 spans | ~93 MiB |
| 50,000 spans/s | 250,000 spans | ~460 MiB |

At ordinary rates this is small. The number to size is the **bound**.

The tier alone is not what to size against — compaction and reads have transients of their
own, and they land at the same time on a busy node. Measured peak for the **whole process**
across a full cycle (tier filled, a bounded read, compaction, a cold merge, a SQL aggregate),
at a 1 KiB payload, plus ~114 MiB for a real `serve` process:

| `--max-hot-spans` | spans resident | process peak | vs a 1 GiB limit |
|---|---|---|---|
| 200,000 | 375 MiB | ~668 MiB | 65% |
| 250,000 | 467 MiB | ~760 MiB | 74% |
| **300,000** (default) | **560 MiB** | **~852 MiB** | **83%** |
| 350,000 | 652 MiB | ~945 MiB | 92% |
| 500,000 | 929 MiB | ~1,222 MiB | over |

The default leaves the **1 GiB limit** in the shipped k3s and Helm manifests a margin of
roughly 172 MiB for a query heavier than the one measured. Drop to 250,000 if you would
rather trade ingest backlog for query headroom; go above 300,000 only alongside a larger
memory limit, since 350,000 already reaches 92%.

Three things make up that peak, and they scale differently:

- **The spans**, ~1.9 KiB each at a 1 KiB payload. Scales with `--max-hot-spans`.
- **The read path's hot/cold dedup set**, ~0.07 KiB per resident span. Also scales with the
  bound. A read keys every resident span to decide what a cold block may contribute; the key
  is the ids decoded back to bytes, which is exact and needs no heap.
- **Compaction**, ~155 MiB: one sealed segment cloned plus the Arrow batch built from it.
  This scales with `--seal-threshold`, *not* with the bound, so lowering the seal threshold
  is the way to shrink it.

To set your own: take your p99 span size (the prompt and completion dominate — offloaded
payloads above `--blob-offload-bytes` do not count, they live on disk), budget ~2 KiB per
span at the bound for spans plus dedup, add compaction's ~155 MiB and the process's ~114 MiB,
then leave margin for queries. Going the other way is just as valid: pick the bound from the
limit you already have.

One read is still unbounded by nature: `GET /v1/traces/{id}` and anything that counts the
whole store materialise every matching span. A single trace is small, so this matters only
if you ask for all of a very large store at once.

Watch `evald_hot_spans` against `evald_hot_spans_max`, and `evald_ingest_shedding` for when
the bound is actually biting. Sustained shedding with a healthy compactor means the bound is
too low for your rate; raise it **and** the memory limit together.

Lowering `--compact-interval-secs` shrinks the window directly and costs more, smaller
Parquet blocks — which cold-to-cold merging then collapses, so it is a cheaper knob than it
used to be.

## `evald eval run`

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--config` | — | path | `eval.yaml` | The eval YAML (below). |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Where per-item + aggregate Scores are persisted (and the judge cache lives). Must **not** be a data-dir a live `evald serve` holds open — the redb lock is exclusive (see [OPERATIONS.md](./OPERATIONS.md#troubleshooting)). |
| `--estimate` | — | flag | off | Preview the judge token usage + indicative cost for this config **without any network call**, then exit. Tier-1 evaluators are zero-cost, so with no `judges:` it just says so. |
| `--junit` | — | path | — | Also write a [JUnit XML report](#junit-xml-reports) here: one test case per evaluator, failing when its threshold is missed. |

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
| `--junit` | — | path | — | Also write a [JUnit XML report](#junit-xml-reports): one test case per evaluator delta, failing exactly when the gate above fails. |

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

## `evald suite run`

Runs a declarative suite: several eval configs (`cases:`), each optionally repeated, gated by a
suite-level pass rate.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--config` | — | path | `suite.yaml` | The suite YAML (`cases[]`, `min_pass_rate`, `repeat`, `min_pass`). |
| `--junit` | — | path | — | Also write a [JUnit XML report](#junit-xml-reports): one test case per suite case, plus a final `suite pass rate` case that carries the gate's verdict. |

Exit code: `0` when the suite passes, `1` when it fails.

## JUnit XML reports

`--junit <path>` on `eval run`, `eval compare` and `suite run` writes a standard JUnit XML
file that GitHub, GitLab, Jenkins and most other CI systems render as a test report, so a failed
gate appears as a named, expandable failure. One `<testsuite>` per invocation:

| Command | One test case per | Fails when |
|---|---|---|
| `eval run` | evaluator | its aggregate is below its threshold. An evaluator that scored no items is `skipped`, never a failure. |
| `eval compare` | evaluator delta | the gate the command applied trips on that row (`--fail-on-regression`, with or without `--significance`). The message carries the delta, and the p-value and confidence interval when a significance test ran. An evaluator present on only one side is `skipped`. |
| `suite run` | suite case, plus a final `suite pass rate` | the case missed its `min_pass`; the final case fails when the suite misses `min_pass_rate`. A case can fail while the suite still passes (`min_pass_rate` below `1.0`); the exit code follows the final case. |

- The file is written **even when the gate fails**, and even when the command errors (a missing
  config, an unknown run id): then it holds a single `<error>` test case with the message. The
  exit code is never changed by `--junit`.
- If the report cannot be written and the command otherwise succeeded, the command fails, so a CI
  job that asked for a report does not pass without one. If the command had already failed, the
  write error is only printed and the original failure stands.
- `time` is the wall-clock seconds of the whole command on the suite; individual cases report
  `0.000`, because the evaluators are not timed one by one.
- Ordering follows the command's own report, so the same inputs give the same file. Characters XML
  1.0 forbids (from a model's or judge's output) are replaced with U+FFFD and markup characters are
  escaped, so the file always parses.

## `evald scores export`

Prints stored scores as OpenTelemetry GenAI `gen_ai.evaluation.result` events, one JSON object per
line, oldest first.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--format` | — | string | `gen_ai-event` | Output shape. Only `gen_ai-event` today. |
| `--name` | — | string | all | Only scores with this name. |
| `--limit` | — | int | `10000` | Export at most this many (the newest). |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The score store to read. Takes the data-dir lock, so it cannot run against a live `evald serve` on the same directory. |

Each line:

```json
{"name":"gen_ai.evaluation.result","time_unix_nano":1700000000400000000,
 "trace_id":"…","span_id":"…",
 "attributes":{"gen_ai.evaluation.name":"Relevance","gen_ai.evaluation.score.value":0.75,
               "gen_ai.evaluation.score.label":"relevant","gen_ai.evaluation.explanation":"…"}}
```

`span_id` is present for span-targeted scores and `trace_id` for trace-targeted ones. Run
aggregates and session scores have no operation to attach an event to and are skipped. The same
shape is ingested from span events: see [API.md](./API.md#post-v1traces).

## `evald cost`

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--by` | — | `model` \| `user` \| `session` \| `service` \| `provider` | `model` | Attribution dimension (case-insensitive). Untagged spans surface as `(untagged)`. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The spans to report over. |
| `--limit` | — | int | `100` | Max attribution rows printed. |
| `--price-table` | `EVALD_PRICE_TABLE` | path | *(none)* | **Re-price** the report from the stored token counts under this table (laid over the built-in one) instead of showing the costs stored at ingest. Spans whose cost the instrumentor reported keep it; nothing is rewritten. See [Cost and token semantics](./INSTRUMENTATION.md#cost-and-token-semantics). |

A model with no price shows as `(no price)`, and a `*` marks a total that understates spend because some spans could not be priced.

## `evald latency`

Exact **nearest-rank** p50 / p95 / p99 (and max) of `end − start` over **LLM spans** — the same set the
usage metrics count — plus time to first token where spans carry it. A time to first token is read only from
span attributes (`gen_ai.response.time_to_first_chunk` in seconds; `ai.response.msToFirstChunk`,
`ai.stream.msToFirstChunk`, `time_to_first_token_ms` in milliseconds) and is shown as `unknown`, never
estimated, when absent. The equivalent SQL is in [OPERATIONS.md](./OPERATIONS.md#llm-usage-cost-tokens-latency-quality).

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--by` | — | `model` \| `provider` \| `service` | `model` | Grouping dimension (case-insensitive). `user` and `session` are refused: with a percentile per user most groups hold one or two spans. Untagged spans surface as `(untagged)`. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The spans to report over. |
| `--limit` | — | int | `100` | Max groups printed. |

## `evald query <sql>`

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| *(positional)* `sql` | — | string | *required* | Read-only SQL over the `spans` (hot ∪ cold, deduped) and `scores` tables — the same guard as `POST /v1/sql` (see [API.md](./API.md#post-v1sql)). |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The blocks + score store to query. Takes the redb lock — cannot run against a live `serve` on the same dir. |
| `--limit` | — | int | `1000` | Max rows printed (a truncation note goes to stderr so stdout stays pipeable JSON). |

## `evald compact`

Cold-to-cold compaction on demand: collapse every hour partition holding more than one
block, then every fully-past UTC day. This is what `serve` does on its compaction tick,
except that it ignores the tick's size classes and quiet window — an operator running it
asked for the full collapse now. It takes the redb lock, so run it against a stopped node.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The blocks to merge. |
| `--dry-run` | — | flag | off | Report what would be merged without writing anything. |
| `--max-spans` | `EVALD_COLD_MERGE_MAX_SPANS` | int (spans) | `1000000` | A merged block never exceeds this many spans. |
| `--days` | `EVALD_COLD_MERGE_DAYS` | bool | `true` | Also collapse fully-past UTC days into day blocks. |

## `evald migrate`

Report — and, without `--dry-run`, apply — what this build needs to do to a data-dir
written by an older evald. Today that is stamping the `FORMAT` marker on a directory
written before the format freeze; no data is rewritten. Exits non-zero if the directory
was written by a **newer** format than this build reads. See [FORMAT.md](./FORMAT.md).

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The data-dir to inspect. |
| `--dry-run` | — | flag | off | Report what would change without writing anything. |

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

The on-disk format is **versioned and frozen at format 1**. Every data-dir carries a
`FORMAT` marker, a release reads its own format and the one before it, and a data-dir
written by a newer evald is refused rather than opened optimistically. The full
contract — every file, the Parquet metadata each block carries, and what counts as a
breaking change — is [FORMAT.md](./FORMAT.md). In brief:

- **WAL** (`wal/<seqno>.wal`): CRC-framed records, `[u32 len][u32 crc32][JSON payload]`.
  Internal ([`src/store/wal.rs`](../src/store/wal.rs)) — a WAL segment is a transient
  staging area, not an archive, so do not build tooling against it.
- **Cold blocks** (`blocks/YYYY/MM/DD/HH/*.parquet`, plus merged blocks): plain
  Snappy-compressed Parquet with a flat, typed columnar schema
  ([`src/store/cold.rs`](../src/store/cold.rs) `schema()` — `trace_id`, `span_id`,
  `model`, token counts, `cost_usd`, `raw_attributes` as a JSON string column, …).
  Deliberately an **open format**: DuckDB / pandas / external DataFusion read the
  files directly off disk. Each block's Parquet footer also records what wrote it and,
  for a merged block, which blocks it replaced.
- **Indexes / scores** (`index.redb`, `scores.redb`, `judge_cache.redb`): redb
  databases, JSON-serialized values. Internal — access them through evald.
- **Upgrades** are a binary swap; `evald migrate` reports and applies what a data-dir
  needs (which, for a directory written before the freeze, is a marker and nothing
  else). See [OPERATIONS.md § Upgrade](./OPERATIONS.md#upgrade).
