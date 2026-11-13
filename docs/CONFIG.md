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
| `warn` | Quietest useful setting. What the throughput numbers in the [README's Performance section](../README.md#performance) assume. |

**The per-span line is `debug` on purpose, and it is not free to turn on.** Formatting
and colouring its ~14 fields measured **~38,000 instructions per span, about a third of
the ingest hot path**; end to end on a fsync-bound 4-vCPU VM it still cost **+14% of the
server's CPU per span and −7% throughput**. It is off by default and does not belong on
a loaded server. Turning the default *down* to `warn` buys nothing — the two measured
within 0.5% of each other. See [OPERATIONS.md § Logs](./OPERATIONS.md#logs).

## Outbound calls: `--offline`, `--egress-allow`

Two flags every command takes. The default build has no HTTP client at all, so it makes no
outbound call whatever they say; in the `judge` build they decide what the judge (in `eval run`,
`serve --online-eval`, `judge probe`, `doctor --judge-config`) may call:

| Flag | Env var | What it does |
|---|---|---|
| `--offline` | `EVALD_OFFLINE` | No outbound connection at all: no HTTP client is built, no name is resolved. A judge that must call out fails with an error naming the flag; one whose scores are all cached still runs. |
| `--egress-allow` | `EVALD_EGRESS_ALLOW` | Only these destinations (repeatable or comma-separated): a host name (matched whole, case-insensitively), an IP address, or a CIDR (`10.0.0.0/8`, `fd00::/8`). A host is resolved when its client is built and admitted by its name or by an address it resolved to; the client is then pinned to that address, so a DNS change cannot move it, and redirects are not followed. `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` are ignored: a proxy is a host the list does not name, and it would carry the call anywhere. Anything else is refused before it connects (exit `2`). Without the flag, those proxy variables are honoured. |

Giving both is refused (`--offline` would make the list meaningless). Every request that goes out
and every one refused is recorded in the **egress ledger** ([`evald egress report`](#evald-egress-report)).

## Exit codes

Every command follows one contract, so a CI step can tell a regression from a broken setup:

| Code | Meaning |
|---|---|
| `0` | Done; every gate passed. |
| `1` | A gate failed: a threshold missed (`eval run`), a regression (`eval compare --fail-on-regression`), a failed suite (`suite run`), a judge that needs recalibration (`eval calibrate --fail-on-divergence`), a failed `doctor` check. |
| `2` | evald could not do what was asked: an invalid flag or address, a missing or malformed config or dataset, a store it cannot open. The message on stderr starts with `Error:`. |

Before 0.4.0, the `2` cases also exited `1`. A script that tests for a non-zero exit is
unaffected.

`evald serve` drains and exits `0` on `SIGTERM` (what `docker stop`, a Kubernetes pod
deletion and systemd send) as it does on Ctrl-C.

## `evald serve`

Runs the OTLP/HTTP receiver + durable store + query/SQL API + embedded SPA.

| Flag | Env var | Type | Default | What it does / when to change it |
|---|---|---|---|---|
| `--otlp-http` | `EVALD_OTLP_HTTP_ADDR` | `host:port` | `127.0.0.1:4318` | Bind address for the whole HTTP surface (OTLP ingest, `/v1/*` API, SPA). Loopback by default — it never listens on the network unasked; binding wider is a deliberate act (see [OPERATIONS.md § Security posture](./OPERATIONS.md#security-posture)). |
| `--otlp-grpc` | `EVALD_OTLP_GRPC_ADDR` | `host:port` | `127.0.0.1:4317` | Bind address for the OTLP/gRPC receiver (traces and logs, behind the same auth gate). Empty disables gRPC ingest. Loopback by default, like `--otlp-http`. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Directory for all durable state: `wal/`, `blocks/`, `index.redb`, `scores.redb` (layout in [OPERATIONS.md](./OPERATIONS.md#what-the-state-is----data-dir-layout)). Created if absent. |
| `--seal-threshold` | `EVALD_SEAL_THRESHOLD` | int (spans) | `50000` | Seal the active WAL segment after this many spans; sealed segments become eligible for compaction to Parquet. Lower it to get smaller, more frequent Parquet blocks (and a smaller WAL replay on restart); raise it for fewer, larger blocks. |
| `--compact-interval-secs` | `EVALD_COMPACT_INTERVAL_SECS` | int (seconds) | `5` | Background compaction interval. `0` disables background compaction entirely — spans then stay in the WAL + hot tier (reads still see them; the WAL is never truncated). |
| `--cold-merge-threshold` | `EVALD_COLD_MERGE_THRESHOLD` | int (blocks) | `16` | Merge an hour partition's cold blocks once this many of a similar size have accumulated. Every sealed segment becomes a block, so without merging the block count only grows — and the file set a query opens with it. `0` turns merging off on the compactor's tick; `evald compact` still merges on demand. |
| `--cold-merge-max-spans` | `EVALD_COLD_MERGE_MAX_SPANS` | int (spans) | `1000000` | A merged block never exceeds this many spans; a partition with more becomes several blocks. Keeps retention (which drops blocks whole) and merge memory granular. |
| `--cold-merge-days` | `EVALD_COLD_MERGE_DAYS` | bool | `true` | Also collapse each fully-past UTC day into day blocks, so a low-volume store's block count grows per day rather than per hour. |
| `--cold-merge-day-quiet-secs` | `EVALD_COLD_MERGE_DAY_QUIET_SECS` | int (seconds) | `3600` | A closed day collapses only once nothing has been written into it for this long, so a backfill (or a client with a skewed clock) does not have the whole day rewritten on every tick. |
| `--cold-merge-grace-secs` | `EVALD_COLD_MERGE_GRACE_SECS` | int (seconds) | `60` | How long a merge input stays on disk after it leaves the index, so a query that listed it moments before the merge can still read it. Raise it above your slowest query if a long scan ever races a merge. |
| `--max-hot-spans` | `EVALD_MAX_HOT_SPANS` | int (spans) | `300000` | Durable-backlog bound: shed ingest (`429 + Retry-After`) once this many un-compacted spans are resident in memory. Everything accepted is already durable in the WAL, so this bounds **memory**, not durability — it stops a lagging compactor from OOMing the process. `0` disables the bound. **Size it against your container limit** — see below. |
| `--blob-offload-bytes` | `EVALD_BLOB_OFFLOAD_BYTES` | int (bytes) | `262144` | Offload a span's input/output payload larger than this to the blob store, leaving a compact `evald-blob:<key>` reference on the span (fetch it with `GET /v1/blobs/{key}`). Keeps a large RAG context or tool output out of the WAL, the Parquet blocks and query responses. `0` keeps payloads inline. |
| `--auth-token` | `EVALD_AUTH_TOKEN` | string (repeatable) | *(none)* | Require `Authorization: Bearer <token>` on **every** request — HTTP (OTLP ingest, `/v1/*`, SPA; `401` on failure) and OTLP/gRPC (`authorization` metadata; `UNAUTHENTICATED` on failure). Each flag occurrence is **one whole token** (a comma is part of the token); repeat it for several accepted tokens (rotation / per-client revocation). `EVALD_AUTH_TOKEN` is a **comma-separated** list, **unioned** with the flag(s) and the file — not overridden. Tokens must be **≥16 printable-ASCII chars** (rejected at boot). **Nothing set — or a present-but-empty value — ⇒ auth OFF** (the default local posture). Arm it before exposing evald on a shared/public network (see [OPERATIONS.md § Security posture](./OPERATIONS.md#security-posture)). Shared-secret gate, **not** TLS. |
| `--rollup` | `EVALD_ROLLUP` | `name=fn` (repeatable, comma-separated) | *(defaults)* | How a score name combines when [`GET /v1/traces/{id}/scores`](./API.md#get-v1tracestrace_idscores) rolls span scores into a trace-level value: `mean` \| `min` \| `max` \| `sum` \| `all` \| `any`. Defaults: `mean` for numeric, `all` for boolean. **A CI gate usually wants `min`** — one bad step in a ten-step agent should fail the trace, and a mean dilutes it. Read-time only: nothing is rewritten, and changing this changes the answer for traces already stored. |
| `--price-table` | `EVALD_PRICE_TABLE` | path | *(built-in table)* | A model price table (the LiteLLM `model_prices_and_context_window.json` shape, or any subset of it) laid over the built-in one; it wins for every model it names. Used at ingest to fill `cost_usd` for spans that carry a model and token counts but no cost of their own. Re-read when the file's modification time changes (a bad edit keeps the previous table); a file that does not parse stops `serve` at start-up. evald makes no network call to update it. Logs a warning when the table is older than 90 days. |
| `--no-usage-metrics` | `EVALD_NO_USAGE_METRICS` | flag | off | Do not record the LLM usage series (cost, tokens, latency, rolling evaluator scores) on `/metrics`. They are on by default: one lock per committed batch and a bounded number of series. evald's own health series are unaffected. See [API.md § LLM usage series](./API.md#llm-usage-series). |
| `--metrics-model-cap` | `EVALD_METRICS_MODEL_CAP` | int | `100` | Distinct model names carried as the `gen_ai_request_model` label before the rest fold into `other` (counted by `evald_usage_labels_folded_total`). Bounds the series count, and so the scrape size (~4–5 KB per series) and the scraper's memory. Providers (16) and services (32) have fixed caps, and there are at most 2048 label tuples. |
| `--logs` | `EVALD_LOGS` | `genai` \| `all` \| `off` | `genai` | What the OTLP logs door (`POST /v1/logs`, gRPC `LogsService`) keeps. `genai`: records evald recognises (`gen_ai.evaluation.result`, Claude Code, Codex CLI and Gemini CLI events) become spans and scores; every other record is acknowledged, counted (`evald_log_records_total{outcome="unmapped"}`) and dropped, so an application's ordinary logs never fill the store. `all`: those are kept too, each as a zero-duration span with its attributes, body and severity, in the record's own trace or, without one, a trace of its own: useful to see what a new tool sends, but it makes evald a log store. `off`: every record is acknowledged and dropped. See [API.md § POST /v1/logs](./API.md#post-v1logs). |
| `--mcp-allow-writes` | `EVALD_MCP_ALLOW_WRITES` | flag | off | Let MCP clients on [`POST /mcp`](./API.md#post-mcp) write scores (the `add_score` tool). Without it a connected agent can only read. |
| `--online-eval` | `EVALD_ONLINE_EVAL` | path | unset | Judge a sample of the model calls this server ingests, per the spec file ([below](#online-sampled-evals)). Off unless given. Needs `--features judge`; without it `serve` refuses to start (exit `2`). |
| `--redact` | `EVALD_REDACT` | classes, comma-separated, or `all` (repeatable) | *(none)* | **Redact sensitive values before anything is written.** Classes: `email`, `credit_card` (Luhn-checked), `ssn`, `phone`, `ip`, `api_key`, `jwt`. Applies to prompts (`input_value`), completions (`output_value`) and every string in `raw_attributes` (recursing into nested JSON). **Unset ⇒ no redaction** — the rewrite is irreversible, since the raw value never reaches the WAL, a blob or a Parquet block, so it is always an explicit choice. Not scanned: `user_id`, `session_id`, `service_name`, span name — see [OPERATIONS.md § Redaction](./OPERATIONS.md#redaction). |
| `--redact-action` | `EVALD_REDACT_ACTION` | `redact` \| `hash` \| `drop` | `redact` | `redact` → `[REDACTED:<class>]`. `hash` → `[<class>:<16 hex>]`: unrecoverable, but **equal values hash equally**, so "how many distinct users" stays answerable without the store holding the value. `drop` removes the match entirely. |
| `--redact-custom` | — | `name=regex` (repeatable) | *(none)* | An extra rule, e.g. `employee_id=EMP-[0-9]{6}`. Compiled at startup, so a bad pattern fails the process rather than silently never matching under load. |
| `--retention` | `EVALD_RETENTION` | window (`30d`/`72h`/`90m`/`3600s`) | *(none)* | Automatically drop cold Parquet blocks whose spans **all** predate `now − window`, on a timer. **Unset ⇒ no automatic deletion** — evald never removes data unless asked. A block is dropped whole (index entry first, then `unlink`), so reclamation is `O(unlink)` with no row-by-row delete and no vacuum. `evald retention --dry-run` previews the same sweep. |
| `--retention-interval-secs` | `EVALD_RETENTION_INTERVAL_SECS` | int (seconds) | `3600` | How often the automatic sweep runs. Ignored when `--retention` is unset. |
| `--disk-min-free` | `EVALD_DISK_MIN_FREE` | bytes (`512MiB`, `2g`, or a count) | `256MiB` | **Disk floor.** While the data-dir filesystem has less than this free, ingest is refused with `503 + Retry-After: 30` — a clean refusal *before* a write hits `ENOSPC` mid-segment. `0` disables the floor. Measured as space available to an unprivileged process (`statvfs` `f_bavail`), so the root reserve is not counted as usable. |
| `--disk-warn-free` | `EVALD_DISK_WARN_FREE` | bytes | `1GiB` | Log a warning once free space falls below this, while still accepting ingest — the signal before the floor. `0` disables. Must be **greater than** `--disk-min-free`, or `serve` refuses to start (a warning that can never fire before the floor is worse than none). |
| `--disk-check-interval-secs` | `EVALD_DISK_CHECK_INTERVAL_SECS` | int (seconds) | `10` | How often free space is sampled. `0` disables the guardrail entirely — no probe, no floor. Sampling is on a timer, not per request, so an append pays one relaxed atomic read. The guardrail **fails open**: if free space cannot be read (a non-Unix target, or a failing probe) it never blocks ingest, and says so once in the log. |
| `--auth-token-file` | `EVALD_AUTH_TOKEN_FILE` | path | *(none)* | A file of bearer tokens — one per line; blank lines and `#` comments ignored — unioned with any `--auth-token` values. Keeps secrets out of argv/env; rotate by editing the file. |

### Online sampled evals

`evald serve --online-eval spec.yaml` judges a sample of the live model calls it ingests, with
the same judges and endpoints as an eval config, and writes each result as an ordinary score
on the call's span (`source = eval`, `config_id = 'online'`). Example:
[`examples/eval/online.yaml`](../examples/eval/online.yaml). Source:
[`src/online.rs`](../src/online.rs).

**Ingest never waits on the judge.** The ingest path picks spans (a hash and a few comparisons
per model call) and, once the batch is durable, offers them to a bounded queue without
waiting. A full queue sheds the span and counts it (`evald_online_eval_shed_total`); a slow or
failed judge call is a metric and a log line, never ingest latency or an ingest error. The judge
runs on a thread and runtime of its own, and sees what was stored: redacted, the same as every
other reader. Measured on one 4-CPU machine, 6 interleaved rounds of the same release binary
with the worker off and on (5 % sampled, a judge taking 50 ms a call, so the queue was full and
shedding throughout): 49.6k vs 48.9k spans/s at 32 connections × 10 spans (−1.4 %), 33.8k vs
32.8k at 32 × 100 (−3.0 %, the ranges overlapping). The cost is the sampling and the worker's
score writes sharing the disk with ingest.

Which calls are judged: model calls (`is a model call`, as `evald cost` counts them) that have
an output, are not evald's own judge calls, and pass `only:`. Of those, every call that errored
and every call over `always.cost_usd_over`, and the rest at `sample_rate`, decided by a hash of
the span id, so a span re-sent by an exporter gets the same answer. Each picked call is judged by
every judge in the spec; a rail that needs a field the call lacks (`expected_output`, `context`)
is counted `unscorable`.

| Key | Type | Default | What it does |
|---|---|---|---|
| `endpoints` | list | `[]` | As in an eval config ([`endpoints:` entries](#endpoints-entries)). The endpoint's `max_in_flight` (default `1`) caps the judge's concurrent calls to it. |
| `judges` | list | *required* | As in an eval config, but each **must** name an `endpoint:`; `provider:` is refused (an online judge has no implicit endpoint). |
| `sample_rate` | float in [0, 1] | `0.01` | Share of the eligible calls judged. |
| `always.errors` | bool | `true` | Judge every call whose span status is an error. |
| `always.cost_usd_over` | float | unset | Judge every call that cost at least this. |
| `only.services`, `only.models` | lists | any | Only calls from these services / with these models. |
| `daily_token_cap` | integer > 0 | *required* | Judge tokens (input + output, as the server reports them, else estimated) one UTC day may spend. Once spent, no new call starts until the next day (`outcome="budget"`). A call already in flight finishes, so the day can end one call over. |
| `queue` | integer > 0 | `256` | Picked calls waiting for the judge before new ones are shed. |
| `pause.p95_ms_over` | float | unset | Pause judging while the p95 duration of recent production calls **to the judge's own server** is over this (`outcome="paused"`). |
| `pause.window` | integer ≥ 20 | `200` | How many recent calls that p95 is over. |

**When the judge shares production's server.** A model call evald ingests that went to a judge
endpoint's own host and port (its `server.address` / `server.port`) means the judge competes
with production for the same GPU. evald logs that once and sets `evald_online_eval_colocated` to
`1`. Use a separate endpoint; or set `pause:` so judging stops while those calls are slow (it
needs at least 20 of them in the window before it pauses).

What a judged call leaves behind: the score on the span, and the judge's own call as an
`evald.judge` span (one trace per UTC day, `evald.judge.target_span_id` naming the span it
judged), so the judge's spend is in `evald cost --by service`. The scores are advisory: nothing
gates on them. Before trusting one judge's online scores, measure it: `evald judge probe`, and
`eval calibrate` against human labels and over repeat runs ([`eval calibrate`](#evald-eval-calibrate)).

```bash
evald query "SELECT s.model, AVG(sc.num_value) AS relevancy, COUNT(*) AS judged
             FROM scores sc JOIN spans s ON s.span_id = sc.target_id
             WHERE sc.config_id = 'online' GROUP BY s.model"
```

Its series on `/metrics` are listed in [API.md § `/metrics`](./API.md#get-metrics).

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
| `--no-cache` | — | flag | off | Judge every item again instead of serving cached judge scores (new scores still replace the cached ones). A repeat run, for `eval calibrate --runs` to measure the judge's stability over. |
| `--report-json` | — | path | — | Also write the run as a [versioned JSON report](#json-reports) (`evald.report/v1`), for a pipeline to read. |

Besides the per-item scores (on the item's span or trace, when it names one) and each
evaluator's aggregate, a run stores every scored item's value under the item's key (its `id`,
else its `span_id`, else its `trace_id`) as one score per evaluator, `evald.run.items`, on the
run. That is what `eval compare` pairs and `eval calibrate --runs` matches. Give dataset items an
`id` to get both.

Exit code: `0` when every configured threshold is met, `1` on a threshold
regression — the CI gate — and `2` when the run could not happen ([exit codes](#exit-codes)).

## `evald eval compare <run_a> <run_b>`

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Where the two runs' aggregate Scores are read from. |
| `--fail-on-regression` | — | flag | off | Exit non-zero when any shared evaluator dropped (B below A) beyond `--tolerance`. |
| `--tolerance` | — | float ≥ 0 | `0.0` | How far an evaluator may drop before it counts as a regression. Negative / non-finite values are rejected at parse time. |
| `--significance` | — | flag | off | Gate only on *statistically significant* regressions: a drop fails the build only when the t-test shows the `(1 − alpha)` CI on the delta excludes 0. A regression that **cannot** be tested (a run persisted without stats, n < 2, or every item moved by exactly the same amount) still gates — it is not forgiven. |
| `--alpha` | — | float in (0, 1) | `0.05` | Significance level for `--significance` and the printed CI (`0.05` → 95 % CI). |
| `--junit` | — | path | — | Also write a [JUnit XML report](#junit-xml-reports): one test case per evaluator delta, failing exactly when the gate above fails. |
| `--non-inferior` | — | flag | off | Gate on [non-inferiority](#non-inferiority---non-inferior---margin): exit `1` unless every shared evaluator is shown no worse in run B than in run A by more than `--margin`. Needs `--margin`. |
| `--margin` | — | float > 0 | *required with `--non-inferior`* | The largest drop (on the evaluator's own 0–1 scale) you accept as "no worse". Never defaulted. |
| `--price-table` | `EVALD_PRICE_TABLE` | path | — | Price both runs' replays at this table's base rates (over the built-in one), from their recorded tokens. |
| `--report-json` | — | path | — | Also write the comparison as a [versioned JSON report](#json-reports): every delta, test, verdict and gate. |

Which t-test: when both runs scored exactly the same items (by item key, above), the **paired**
test on the per-item differences, with a percentile bootstrap interval (2,000 resamples, a fixed
seed, so the same runs print the same interval) as a check that does not assume normal scores.
Pairing removes the items' own spread from the noise, so a drop on the same items is seen that
an unpaired test would call noise. Otherwise **Welch's** two-sample test on the runs' statistics,
and the line under the table says why the runs could not be paired (items without an `id`, a
different item set, repeated ids, or a run written before per-item values were recorded):

```text
evaluator          run_a     run_b     delta  change  p-value            95% CI Δ verdict
exact_match        0.500     0.375    -0.125  DOWN     0.0234     [-0.232,-0.018]  signif

  exact_match    paired t on 40 items scored in both runs; bootstrap 95% CI [-0.250,-0.025]
```

The same scores without item ids: `Welch (unpaired): 40 item(s) have no id`, p = 0.27, noise.

**When the judge changed.** A judge's scores move when its weights do, so a drop between two
runs can be the judge's rather than the system's. `eval compare` reads both runs' manifests
([below](#which-weights-answered)) and, for each judge in both, names what differs: its
`weights` (the digest where the server gave one, else the model id it listed), `quantization`,
the `model` name it asked for, its `base_url`, or the `judge_version` of evald's judge prompts.
Here the same Ollama tag was pulled again over new weights between the runs:

```text
evaluator          run_a     run_b     delta  change
judge_toxicity     0.900     0.600    -0.300  DOWN

warning: a judge changed between the runs, so its deltas are the judge's as much as the system's:
  judge_toxicity weights       sha256:aaaaaaaaaaaa… -> sha256:bbbbbbbbbbbb…

REGRESSION — 1 evaluator(s) dropped > 0.000 from run A to run B.
```

The warning does not change the gate: a regression still fails `--fail-on-regression`, because
which side moved is for you to decide (rerun run A's judge on the new weights, or pin the
digest). Runs written before 0.5 have no provenance in their manifest; runs with no manifest
are compared without the check. The MCP `compare_runs` tool returns the same list as
`judge_changes`.

### Non-inferiority: `--non-inferior --margin`

"No significant difference" is absence of evidence, and a small dataset gives it for free. The
question a migration asks is different: **is B no worse than A by more than δ?** With
`--non-inferior --margin δ`, `eval compare` answers it with a one-sided paired test on the items
both runs scored (`H0: mean(b − a) ≤ −δ`), at level `--alpha`:

- **non-inferior**: the one-sided `(1 − alpha)` lower bound on the mean difference is above `−δ`;
- **inferior**: even the upper bound is below `−δ`, so B is shown worse by more than δ;
- **inconclusive**: neither, and the gate fails. More items narrow the bounds; a wider margin is
  a decision you would have to defend.

`--alpha 0.05` gives a one-sided 95 % bound; `--alpha 0.025` matches the convention of reading the
lower end of a two-sided 95 % interval. The margin is on the evaluator's own scale (a pass/fail
evaluator: `0.02` is two items in a hundred) and has no default: what loss is acceptable is yours
to decide. Every evaluator in both runs gets a verdict over all its paired items, which is what
gates, and one per **slice**: the value of each item `metadata` key the run recorded
([`slice_by:`](#top-level), by default `language`, `route` and `task`). Slices are reported, not
gated. A slice that matters on its own (one language you serve) is gated by giving it a dataset of
its own. The runs must have scored the same items, by item `id`, or the row says why not.

Two replays of 200 items (`evald replay`, below), an exact-match evaluator: B got two `ms`
items wrong. The paired t-test calls that noise; at a 0.02 margin it is not shown non-inferior:

```text
evaluator          run_a     run_b     delta  change  p-value            95% CI Δ verdict
exact_match        1.000     0.990    -0.010  DOWN     0.1578     [-0.024,+0.004]   noise

  exact_match    paired t on 200 items scored in both runs; bootstrap 95% CI [-0.025,+0.000]

replay         run_a                          run_b                          delta
endpoint       hosted (gpt-4o-mini)           gpu-box (qwen2.5:7b)
jurisdiction   US                             MY
calls          200                            200
tokens in/out  4000 / 400                     5000 / 400
cost           $0.0008                        $0.0003                        -0.0005 $
latency p50    33 ms                          15 ms                          -18 ms
latency p95    35 ms                          17 ms                          -18 ms

non-inferiority: is run B no worse than run A by more than 0.02? (paired, one-sided 95% lower bound)
evaluator              slice                 pairs    mean Δ  lower bound  verdict
exact_match            all                     200    -0.010      -0.0217  inconclusive
exact_match            language=en             120    +0.000      +0.0000  non-inferior
exact_match            language=ms              80    -0.025      -0.0542  inconclusive
NOT SHOWN NON-INFERIOR — exact_match (inconclusive). Inconclusive is not a pass: more items, or a wider margin you can defend, decide it.
```

The `replay` block appears when either run was an `evald replay`: where each ran (endpoint,
model, the endpoint's `jurisdiction:` label), its tokens, its cost at the price table's rates
(`unpriced` for a model the table does not know: give a self-hosted one its GPU rate in a
`--price-table` entry, as [`evald cost gpu-rate`](#evald-cost-gpu-rate) prints it, and
`eval compare --price-table` prices both replays from their recorded tokens at its rates), and
its latency. A JUnit report (`--junit`) gets one `non-inferior: <evaluator>` case per evaluator,
failing as the gate does; the MCP `compare_runs` tool takes a `margin` and returns the same
verdicts. A walk-through of the whole gate: [MIGRATING.md](./MIGRATING.md).

Exit code: `0`, or `1` when `--fail-on-regression` is set and the (raw or
significance-mode) gate trips, or `--non-inferior` is set and an evaluator is not shown
non-inferior, or `2` when the comparison could not run.

## `evald replay`

Sends an eval config's dataset prompts to another endpoint and scores the answers with the
config's own evaluators and judges, as a run of its own: the candidate run of a migration gate
(`--features judge`). Each item's `input` is the user message, sent at temperature 0 with a
fixed seed and no system message or response format; each item needs an `id` (to pair with the
baseline run) and an `input`, and a dataset where one does not is refused before any call. The
calls go through the [egress door](#outbound-calls---offline---egress-allow) as purpose `replay`,
up to the endpoint's `max_in_flight` at once with its `retries`, and each is stored as a model
call span (`service.name = evald.replay`, the prompt and answer as `input.value` /
`output.value`), so its cost and latency are in `evald cost` and `evald latency` too. The run's
per-item scores go on those spans (not on the baseline's), its manifest records the replay
(endpoint, model, [provenance](#which-weights-answered), tokens, cost, latency) as `replay`, and
it prints like `eval run`, with a line for the replay:

```text
replayed on gpu-box (http://gpu-box:8000), model qwen2.5:7b
  200 call(s), 5000 input + 400 output tokens, $0.0003, latency p50 15 ms, p95 17 ms
```

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--config` | — | path | `eval.yaml` | The eval config: dataset, evaluators, judges, thresholds. |
| `--endpoint` | — | string | — | Replay on this endpoint from the config's `endpoints:` (or `openai` / `anthropic`). |
| `--base-url` | — | URL | — | Or a server given directly, with or without `/v1`. |
| `--api` | — | `openai-chat` \| `anthropic-messages` | `openai-chat` | The API `--base-url` speaks. |
| `--api-key-env` | — | string | unset | The variable holding `--base-url`'s key (unset: keyless). |
| `--model` | — | string | *required* | The model to send the prompts to. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | Where the candidate run is stored. |
| `--out` | — | path | — | Also write the candidate dataset (each item with the replay's answer as its `output`) as JSONL. |
| `--no-cache` | — | flag | off | Judge every candidate item again. |
| `--price-table` | `EVALD_PRICE_TABLE` | path | — | A price table over the built-in one, to price the replay's calls. |

Exit code: as `eval run`: `0` when the config's thresholds pass, `1` when one is missed, `2`
when the replay could not run (a call that failed after its retries, a refused egress, a
dataset that cannot pair).

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
| `--pass-threshold` | — | float in [0, 1] | `0.5` | The pass/fail line for the agreement and stability figures (a score at or above it passes, judge and human alike). |
| `--runs` | — | run ids, comma-separated | — | Repeat runs of this judge on the same items: report its stability across them (below). Two or more. |

Besides bias and correlation, calibration reports **pass/fail agreement** with the human at
`--pass-threshold`, and Cohen's kappa for it: the agreement left after what the two sides' own
pass rates would produce by chance. A judge that passes everything agrees 90 % with a set the
human passed 90 % of, and its kappa is 0.

**Repeat stability** (`--runs`): the same judge on the same items several times. Make the runs
with `eval run --no-cache` (a cached rerun repeats the first answers and shows nothing), then:

```text
$ evald eval calibrate --judge judge_qa_correctness --runs "$RUN1,$RUN2,$RUN3"
Stability: judge `judge_qa_correctness` over 3 runs
  items in every run  : 4
  same verdict        : 50.0% of items in every run (score ≥ 0.50)
  Fleiss' κ           : +0.111 (slight)
  mean per-item SD    : 0.2887
  widest spread       : 1.0000
    q3: 1.00 0.00 1.00
    q4: 0.00 0.00 1.00
```

Items are matched by key, so the runs need item ids. It reports the share of items whose verdict
was the same in every run, Fleiss' kappa over the verdicts (the runs as raters), the mean
per-item standard deviation, the widest spread, and the five least stable items. A judge whose
verdicts move between identical runs gives one run's score less weight than its decimals
suggest; `evald judge probe` shows the same thing on one question.

## `evald suite run`

Runs a declarative suite: several eval configs (`cases:`), each optionally repeated, gated by a
suite-level pass rate.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--config` | — | path | `suite.yaml` | The suite YAML (`cases[]`, `min_pass_rate`, `repeat`, `min_pass`). |
| `--junit` | — | path | — | Also write a [JUnit XML report](#junit-xml-reports): one test case per suite case, plus a final `suite pass rate` case that carries the gate's verdict. |

Exit code: `0` when the suite passes, `1` when it fails, `2` when it could not run.

## The GitHub Action

The repository is also a GitHub Action. It installs a pinned evald (the verified
`install.sh`), runs `eval run` or `suite run` with a JUnit report, writes the run's output to
the job summary, and fails the job exactly when evald's exit code says to
([exit codes](#exit-codes)).

```yaml
jobs:
  evals:
    runs-on: ubuntu-latest        # or macos-*; there is no Windows installer
    steps:
      - uses: actions/checkout@v4
      - id: gate
        uses: lucheeseng827/evald@v0.5.0
        with:
          config: evals/eval.yaml   # required
          # command: suite          # `eval` (default) or `suite`
          # version: v0.5.0         # pin the evald release; `latest` follows the newest
      - if: always()
        uses: actions/upload-artifact@v4
        with: { name: evald-junit, path: "${{ steps.gate.outputs.junit }}" }
```

| Input | Default | What it does |
|---|---|---|
| `config` | *(required)* | The eval or suite YAML. |
| `command` | `eval` | `eval` (`evald eval run`) or `suite` (`evald suite run`). |
| `version` | `latest` | The evald release to install. Pin it for a gate that does not move under you. |
| `data-dir` | `.evald-data` | Where `eval run` keeps its scores (cache it between runs to compare against the last one). |
| `junit` | `evald-junit.xml` | The JUnit report's path (also the `junit` output). |
| `args` | — | Extra arguments, split on whitespace, e.g. `--estimate`. |

Outputs: `exit-code` (0, 1 or 2) and `junit`.

## JSON reports

`--report-json <path>` on `eval run` and `eval compare` writes one JSON document a pipeline
reads instead of parsing the printed table. Its `schema` is `evald.report/v1`, and
[`docs/schemas/report-v1.schema.json`](./schemas/report-v1.schema.json) is the JSON Schema it
validates against. Within v1, fields are only added, so a consumer must ignore fields it does
not know; a field renamed, removed or retyped is v2 ([STABILITY.md](./STABILITY.md)). The
report is written whether the gate passes or fails. When the command could not run, the report
is `{"passed": false, "error": "…"}`.

```json
{
  "schema": "evald.report/v1", "kind": "eval_compare", "evald_version": "0.5.0", "passed": false,
  "run_a": "…", "run_b": "…", "alpha": 0.05,
  "gates": {"fail_on_regression": false, "tolerance": 0.0, "significance": false, "non_inferior_margin": 0.02},
  "evaluators": [{"name": "exact_match", "a": 1.0, "b": 0.99, "delta": -0.01, "n_a": 200, "n_b": 200,
                  "test": "paired", "p_value": 0.1578, "ci_low": -0.024, "ci_high": 0.004,
                  "bootstrap_ci": [-0.025, 0.0], "regressed": true}],
  "judge_changes": [],
  "non_inferiority": [{"evaluator": "exact_match", "slice": null, "verdict": "inconclusive",
                       "pairs": 200, "mean_delta": -0.01, "lower_bound": -0.0217, "upper_bound": 0.0017,
                       "p_value": 0.064, "why": null}],
  "replay": {"a": {"endpoint": "hosted", "model": "gpt-4o-mini", "…": "…"}, "b": {"…": "…"}}
}
```

An `eval run` report has `run_id`, `items`, `evaluators` (each with `mean`, `pass_rate`,
`threshold`, `threshold_met`) and `slices` (each evaluator's mean per `slice_by` value).

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

| `--reprice-as` | — | model name | *(none)* | What these model calls would have cost at a **hosted** model's list price: each call's own token counts priced under that model's entry in the price table (the built-in one, or `--price-table` over it), beside the cost stored for it. The savings question for traffic you serve yourself. |
| `--model` | — | model name (repeatable) | every model call | With `--reprice-as`: only the calls of these models. |

A model with no price shows as `(no price)`, and a `*` marks a total that understates spend because some spans could not be priced.

```text
$ evald cost --reprice-as gpt-4.1-mini --model Qwen2.5-7B-Instruct

evald cost — model calls by model, priced as gpt-4.1-mini ($0.40 / $1.60 per 1M input / output)
model                             calls           tokens      cost_usd  as gpt-4.1-mini
Qwen2.5-7B-Instruct                5000         37568192    (no price)         $18.1495
TOTAL                              5000         37568192    (no price)         $18.1495
```

### `evald cost gpu-rate`

A per-token rate for a model you serve yourself. evald cannot know your utilisation, so it does
not guess one: it takes what the GPUs cost over a window and spreads it over the tokens your own
traffic used in that window (the stored model calls of the models named). The GPUs cost the same
idle or busy, so the rate is the rate *at this traffic*; more traffic lowers it. It prints the
`--price-table` overlay entry to paste, after which `evald cost` prices the model like a hosted
one.

```text
$ evald cost gpu-rate --gpu-hour-usd 2.40 --gpus 2 --model Qwen2.5-7B-Instruct --since 7d --output-ratio 4

evald cost gpu-rate — Qwen2.5-7B-Instruct over the last 168.0 h
  GPUs          : 2 × $2.40/h × 168.0 h = $806.40
  calls         : 4999 (first 2026-10-15 09:14 UTC, last 2026-10-22 09:10 UTC; 100% of the window)
  tokens served : 34955160 input, 2601235 output
  rate          : $17.7777 / $71.1110 per 1M input / output (an output token costs 4 input tokens' worth)
```

(A synthetic week of 5,000 calls. At that traffic two GPUs cost $806 for what `--reprice-as`
above prices at $18 hosted; the rate is what tells you so.)

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--gpu-hour-usd` | — | float > 0 | *required* | What one GPU costs per hour: the rental price, or your amortised figure for owned hardware. |
| `--gpus` | — | int ≥ 1 | `1` | GPUs serving the models. |
| `--model` | — | model name (repeatable) | *required* | The models those GPUs serve, as the calls name them. Several share the rate. |
| `--since` | — | window (`7d`, `24h`, `90m`, seconds) | `7d` | The window the GPUs were paid for, ending now. |
| `--output-ratio` | — | float > 0 | `1` | How many input tokens' worth of GPU time one output token costs (decode is slower per token than prefill). `1` is one blended rate; hosted list prices put output at 3-5× input. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The model calls to count. |

Only model calls count (an agent span's usage is its calls', so it is not counted again), and only
calls that name the model: vLLM's own request span names none ([INFERENCE-SERVERS.md](./INFERENCE-SERVERS.md)),
so the app's span of the call is the one counted. When the calls span less than 90 % of the window
it says so: the GPUs were paid for the whole window, so traffic evald did not see (it started
later, or retention dropped it) makes the rate too high.

### `evald cost language-tax`

Tokens per character of model answers, by the language each answer is in and the model that
wrote it, from the stored calls: what each tokenizer charges for Malay, Thai or Tamil before you
choose a model. `vs en` is the same model's tokens per character in that language over its own
in English.

```text
$ evald cost language-tax --since 30d

evald cost language-tax — tokens per character of model answers, the last 30.0 d
language  name               model                         answers     chars    tokens  tok/char   vs en
en        English            hosted-model                        3       216        51     0.236   1.00×
en        English            regional-model                      3       216        54     0.250   1.00×
ms        Malay/Indonesian   regional-model                      3       226        63     0.279   1.12×
ms        Malay/Indonesian   hosted-model                        3       226        73     0.323   1.37×
ta        Tamil              regional-model                      3       223       159     0.713   2.85×
ta        Tamil              hosted-model                        3       223       405     1.816   7.69×
th        Thai               regional-model                      3       161        73     0.453   1.81×
th        Thai               hosted-model                        3       161       146     0.907   3.84×

24 answer(s) read, 0 too short or mixed to place. …
```

(Two models answering the same support questions in four languages, with token counts set as two
tokenizers might charge.) The answer, not the prompt, is measured: a prompt's tokens include the
system prompt and the chat template, an answer's are the model's own words. A character carries
more in some scripts than others, so read across models within a language; `evald replay` of one
dataset on each candidate ([MIGRATING.md](./MIGRATING.md)) gives every model the same questions to
answer, which makes the comparison exact.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--since` | — | window | `30d` | The window, ending now. |
| `--model` | — | model name (repeatable) | every model | Only these models. |
| `--limit` | — | int | `20000` | The most recent answers to read. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The model calls to read. |

## `evald dataset from-spans`

Turns stored model calls into a JSONL dataset `eval run` reads: each span with a captured answer
(`output.value`) becomes a row with its prompt (`input`), the answer (`output`), its `span_id` and
`trace_id`, and `metadata` (model, provider, service, session, start time).

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The spans to read. |
| `--trace-id` | — | hex | all | Only this trace's spans. |
| `--model` | — | string | all | Only this model's calls. |
| `--limit` | — | int | `1000` | Spans to scan, most recent first. |
| `--out` | — | path | stdout | Where to write the JSONL. |
| `--detect-language` | — | flag | off | Label each row's `metadata.language` with the language its prompt is in (the message text of a JSON chat request, else the prompt as it is), so runs on the dataset report and compare by language. Too little text to place leaves the row unlabelled. |

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

## `evald sessions`

The most recently active sessions, one line each: start, duration, traces, LLM calls and their
errors, distinct tool calls and their failures, model calls per trace (`steps`), a suspected
tool loop (`loop`: three identical tool calls in a row), cost and models. `--json` prints the
[`GET /v1/sessions/{id}`](./API.md#get-v1sessionssession_id) object for each instead, tool
calls by name included. Reads the most recent 50,000 spans and says so when that cut a session
short.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The spans to report over. |
| `--limit` | — | int | `20` | Max sessions printed. |
| `--json` | — | flag | off | JSON instead of the table. |

## `evald demo`

Writes a store to look at before any app sends anything: a Claude Code session (the captured
telemetry the dialect is tested on), three traces from a small RAG app (`support-bot`: chain,
retriever, model call; one call fails) priced from the built-in table, and a
`gen_ai.evaluation.result` score on each successful call. Timestamps are moved to the last half
hour. The data is compiled into the binary; nothing is fetched. It refuses a directory that
already holds data, so it can never mix demo spans into a real store.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | — | path | `evald-demo` | Where to write. Must be absent or empty. Deliberately not `EVALD_DATA_DIR`. |

## `evald doctor`

Checks the setup `serve` would start with and prints a fix for each problem:

- **data dir:** it exists and is writable (probed with a temporary file), or `serve` will create it.
- **persistence:** it is not on ephemeral storage (`/tmp`, `/run`, …).
- **disk:** free space against the default 256 MiB ingest floor.
- **ports:** each address can be bound (one in use usually means an `evald serve` is already
  running there).
- **exposure:** a listener beyond loopback without auth fails.
- **price table:** its age.
- **outbound calls:** whether this build has any network client.

- **judges** (only with `--judge-config`): each endpoint the config's judges call, probed as
  `evald judge probe --config` does, one check per finding (`judge reachable`, `judge model`,
  `judge scores`, `judge format`, `judge stable`, `judge latency`, `judge usage`). This needs
  `--features judge`; without it, the check is a warning that says so.

Without `--judge-config` nothing connects anywhere, and no store is opened either way. Exits `1`
when any check fails, `0` otherwise (warnings included).

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The data dir `serve` would use. |
| `--otlp-http` | `EVALD_OTLP_HTTP_ADDR` | `host:port` | `127.0.0.1:4318` | The HTTP address `serve` would bind. |
| `--otlp-grpc` | `EVALD_OTLP_GRPC_ADDR` | `host:port` | `127.0.0.1:4317` | The gRPC address (empty: disabled). |
| `--judge-config` | — | path | unset | An eval config: probe the endpoints its judges call (this one makes model calls). |

Auth is read from `EVALD_AUTH_TOKEN` / `EVALD_AUTH_TOKEN_FILE`, the variables `serve` reads.

## `evald judge probe`

Can this endpoint judge, and how well? Lists the endpoint's models, then sends the same small
grading question three times, the way a judge would, and reports what it found
(`--features judge`):

```text
$ evald judge probe --base-url http://gpu-box:8000/v1 --model qwen2.5:7b

endpoint probe (http://gpu-box:8000, openai-chat), model qwen2.5:7b
  ok    reachable          GET /v1/models 200 in 8 ms, 1 model(s)
  ok    model              qwen2.5:7b is listed as /models/Qwen2.5-7B-Instruct
  ok    scores             3/3 replies held a score
  ok    structured output  json_object (the server constrains the reply)
  ok    deterministic      3 identical calls, identical replies (1)
  ok    latency            p50 2 ms, max 7 ms
  ok    usage              reported: 180 input, 12 output tokens
```

On vLLM the `model` line names the weights it loaded (its model list's `root`). On Ollama, whose
OpenAI-compatible list only echoes the name, the probe also asks Ollama's own `GET /api/tags`
for the digest and quantisation of the tag:

```text
  ok    model              qwen2.5:7b is listed (digest a1b2c3d4e5f6, Q4_K_M, 7.6B)
```

`structured output` says which `response_format` the server took (a server that refused the
schema stepped down, as a judge run would). `deterministic` warns when three identical calls
scored differently: the endpoint is not repeatable at temperature 0, so a single score from it
is noisier than its value suggests. `usage` warns when the server reports no tokens, which
leaves its calls unpriced. Exits `1` when the endpoint cannot be used as a judge (unreachable,
a call fails, or no reply holds a score), `0` otherwise.

| Flag | Type | Default | What it does |
|---|---|---|---|
| `--config` | path | — | An eval config: probe each endpoint its judges call, with that judge's model. |
| `--endpoint` | string | all | With `--config`: only this endpoint (an `endpoints:` name, or `anthropic` / `openai`). |
| `--base-url` | URL | — | Or probe a server directly, with or without `/v1`. Needs `--model`. |
| `--api` | `openai-chat` \| `anthropic-messages` | `openai-chat` | The API `--base-url` speaks. |
| `--model` | string | — | The model to probe `--base-url` with. |
| `--api-key-env` | string | unset | The variable holding `--base-url`'s key (unset: keyless). |

## `evald egress report`

Every destination evald sent data to in a window, from its own ledger: each request the judge
backend made (`evald.egress` spans in the store: time, endpoint, host, the address connected to,
bytes each way, HTTP status, model, purpose, run) and each one the egress door refused.

```text
$ evald --egress-allow 127.0.0.1 eval run --config local.yaml     # judge on a local server: runs
$ evald --egress-allow 127.0.0.1 eval run --config hosted.yaml    # judge on a hosted API: refused
$ evald egress report --since 30d

evald egress report — the last 30.0 d, from evald's own ledger
host                           address          jurisdiction purpose        calls refused errors       sent   received  first / last (UTC)
127.0.0.1                      127.0.0.1        MY           judge              5       0      0    2.2 KiB      639 B  2026-10-22 09:55 / 2026-10-22 09:55
api.openai.com                 -                US           judge              4       4      0        0 B        0 B  2026-10-22 09:55 / 2026-10-22 09:55
```

It is what evald recorded of the calls it made, a statement of fact rather than a compliance
claim; with the default build's "no HTTP client" check (`scripts/check-no-network.sh`) it is the
evidence a reviewer can check. A run that failed still records the calls it made.

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--since` | — | window (`30d`, `24h`, `90m`, seconds) | `30d` | The window, ending now. |
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The store holding the ledger. |

## `evald connect <tool>`

Prints the configuration that points a tool at this evald, to paste or `eval`. Tools:
`claude-code` (verified against Claude Code 2.1.306), `codex` (an `[otel]` table for
`~/.codex/config.toml`, verified against Codex 0.163.0), `gemini-cli` (alias `gemini`; verified
against Gemini CLI 0.65.0), `otel-app` (any OpenTelemetry-instrumented app), `collector` (an
OpenTelemetry Collector exporter that fans out beside an existing backend).
The endpoints come from the same `--otlp-http` / `--otlp-grpc` values `serve` takes, so a
wildcard bind such as `0.0.0.0` prints as `127.0.0.1`. A token is never printed.

```bash
eval "$(evald connect claude-code)"     # then run claude as usual
evald connect claude-code --settings-json --traces   # a ~/.claude/settings.json env block
evald connect codex >> ~/.codex/config.toml          # if it has no [otel] table yet
eval "$(evald connect gemini-cli)"      # then run gemini as usual
```

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--otlp-http` | `EVALD_OTLP_HTTP_ADDR` | `host:port` | `127.0.0.1:4318` | Where evald serves HTTP. |
| `--otlp-grpc` | `EVALD_OTLP_GRPC_ADDR` | `host:port` | `127.0.0.1:4317` | Where evald serves gRPC. |
| `--host` | — | string | *(from the address)* | The host the tool reaches evald on, when it is another machine. |
| `--http` | — | flag | off | HTTP/protobuf on the HTTP port instead of gRPC. |
| `--traces` | — | flag | off | Also send the tool's spans (Claude Code: the beta span tree of each turn; Codex: its internal spans; Gemini CLI: keeps the spans it otherwise turns off). |
| `--token-env` | — | env var name | *(none)* | Add `Authorization: Bearer ${<var>}`, read from that variable when the tool starts. Codex's `config.toml` and a `settings.json` expand no variables: there, a placeholder for the token itself. |
| `--settings-json` | — | flag | off | Claude Code: a `settings.json` `env` block; Gemini CLI: a `settings.json` `telemetry` block. Instead of shell exports. |

## `evald mcp`

A [Model Context Protocol](https://modelcontextprotocol.io) server on stdio over the store at
`--data-dir`: the tools of [`POST /mcp`](./API.md#post-mcp) (SQL, traces, sessions, cost and
latency reports, eval-run comparison, failing items) for a store **no server holds** — a CI
run's, a copy, `evald demo`'s. The store's lock is exclusive, so for the store a running
`evald serve` holds, point the agent at that server's `POST /mcp` instead. One JSON-RPC message
per line on stdin and stdout; logs go to stderr. Nothing leaves the machine.

```bash
claude mcp add evald -- evald mcp --data-dir ./ci-run-store
```

| Flag | Env var | Type | Default | What it does |
|---|---|---|---|---|
| `--data-dir` | `EVALD_DATA_DIR` | path | `evald-data` | The store to answer from. |
| `--allow-writes` | — | flag | off | Expose `add_score` (a score onto a span, trace or session). |

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
| `--check` | — | flag | off | The upgrade preflight: can **this** evald take over the data-dir? Writes nothing. Exits `1` when it cannot. |

`--check` is for running the new binary against the old one's data-dir before switching
([OPERATIONS.md § Upgrade](./OPERATIONS.md#upgrade)):

```text
$ ./evald migrate --check --data-dir /var/lib/evald
evald migrate --check — can evald 0.5.0 take over /var/lib/evald? (nothing is written)
  ok    format  on-disk format 1, written by evald 0.2.0; evald 0.5.0 reads formats up to 1
  warn  in use  a running evald holds the store: stop it before starting this one (an evald refuses a store another holds)
  ok    disk    3.3 GiB free, the store is 2.0 MiB
ready: evald 0.5.0 can take over this data-dir once the running evald is stopped.
```

`in use` probes the store's lock the way evald takes it, and lets go at once. `disk` warns when
less is free than the store holds, since compaction rewrites blocks. A store written by a newer
evald fails `format`: a data-dir is never downgraded.

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
| `endpoints` | list | `[]` | Where judges' model calls go, each declared once by name (below). |
| `judges` | list | `[]` | Tier-3 LLM-as-judge entries (below). Requires a binary built with `--features judge` to actually call a provider. |
| `thresholds` | map name → float | `{}` | Per-evaluator minimum **mean** score for the run to pass (the CI gate). Keys are evaluator names (`exact_match`, `judge_g_eval`, …). |
| `slice_by` | list of strings | `[language, route, task]` | The item `metadata` keys the run records for slicing: the run's report adds each evaluator's mean per value (for a key with two values or more), and `eval compare --non-inferior` gives a verdict per value. A string value is taken as it is, a number or boolean as its text; an item without the key is in no slice. |

An unknown key at the top level, on a judge or on an endpoint is an error, so a key written in
the wrong place (`base_url:` on a judge) fails the run instead of being ignored.

### `evaluators:` entries (Tier-1, deterministic, no network)

Each entry is `{ type: <snake_case name>, ...params }`. The set is fixed — no
user shell/wasm code. An evaluator that lacks what it needs (e.g. no
`expected_output`) **skips** the item rather than failing it; a fully-skipped
evaluator never trips a threshold.

| `type` | Params | Scores 1.0 when… |
|---|---|---|
| `exact_match` | `normalize` (default `false`) | `output == expected_output` (trimmed). `normalize: true` compares them NFKC-normalised and lower-cased, so full-width forms, ligatures and case do not fail a match ([LANGUAGES.md](./LANGUAGES.md)). |
| `contains` | `substring` (optional; else `expected_output` is the needle) | `output` contains the needle. |
| `contains_all` | `substrings` (non-empty list) | every substring present; the value is the fraction present (partial credit), pass = all. |
| `contains_any` | `substrings` (non-empty list) | at least one substring present. |
| `regex` | `pattern` | `output` matches (invalid patterns are rejected at load). |
| `json_valid` | — | `output` parses as JSON. |
| `json_schema` | `schema` (inline JSON Schema) | `output` parses as JSON **and** validates; non-JSON output is a FAIL, not a skip. Offline — external `$ref`s are not fetched. |
| `non_empty` | — | `output` is non-empty after trimming. |
| `length_bounds` | `min` / `max` (at least one) | char length within `[min, max]`. |
| `levenshtein` | `threshold` (default `0.8`), `normalize` (default `false`) | value = normalized similarity in 0..1; pass when ≥ threshold. `normalize` as `exact_match`'s. |
| `numeric_tolerance` | `tolerance` (≥ 0, default `0`) | both sides parse as numbers and differ ≤ tolerance; skipped when non-numeric. |
| `equals_numeric` | — | both sides parse as numbers and are numerically equal (`"1.0" == "1"`); skipped when non-numeric. |
| `latency` | `max_ms` | item's `metadata.latency_ms` (or `duration_ns`/1e6) ≤ max; skipped when absent. |
| `cost` | `max_usd` | item's `metadata.cost_usd` ≤ max; skipped when absent. |
| `language_match` | `language` (default: the item's `metadata.language`) | 1 when `output` is in the expected language (an ISO 639 code: `ms`, `th`, `ta`, `zh-Hans`), by an embedded detector; Malay and Indonesian count as one. Skipped when no language is expected, or when the output is too short to place (under 20 letters of Latin-script text). The comment names what was detected. [LANGUAGES.md](./LANGUAGES.md). |
| `script` | `script` (default: the script(s) of the item's `metadata.language`), `min` (default `0.8`) | value = the share of `output`'s letters in the expected script (`Latin`, `Thai`, `Tamil`, `Han`, `Arabic` or `Jawi`, `Devanagari`, …; comma-separate several); pass when ≥ `min`. Digits and punctuation do not count. Skipped when no script is expected or the output has no letters. |

### `judges:` entries (Tier-3, BYO-key, `--features judge`)

| Key | Type | Default | What it does |
|---|---|---|---|
| `rail` | `g_eval` \| `qa_correctness` \| `answer_relevancy` \| `faithfulness` \| `hallucination` \| `context_precision` \| `context_recall` \| `toxicity` \| `bias` | *required* | The grading rubric. All rails score higher-is-better in `[0, 1]`. |
| `provider` | `anthropic` \| `openai` | — | A hosted provider's implicit endpoint (configured from the environment, below). Set this **or** `endpoint`. |
| `endpoint` | string | — | The `name` of an `endpoints:` entry. Set this **or** `provider`. |
| `model` | string | *required* | The model id as the endpoint knows it. |
| `criteria` | string | unset | **Required by `g_eval`** — the natural-language grading criteria. |
| `name` | string | `judge_<rail>` | Override the evaluator name (also the `thresholds:` key) so two judges can share a rail. |
| `pass_threshold` | float in [0, 1] | `0.5` | Per-item pass cutoff (score ≥ this counts as a pass). |

### `endpoints:` entries

Where a judge's model calls go: a hosted provider, or the team's own OpenAI-compatible server
(vLLM, Ollama, llama.cpp server, TGI). Source: `EndpointSpec` in
[`src/endpoint.rs`](../src/endpoint.rs).

```yaml
endpoints:
  - name: gpu-box
    api: openai-chat
    base_url: http://gpu-box:8000/v1     # with or without /v1: both mean the same endpoint
    timeout_secs: 120
    max_in_flight: 1
judges:
  - rail: qa_correctness
    endpoint: gpu-box
    model: Qwen/Qwen2.5-7B-Instruct
```

| Key | Type | Default | What it does |
|---|---|---|---|
| `name` | string | *required* | What judges call it. `anthropic` and `openai` are reserved for the implicit endpoints. |
| `api` | `openai-chat` \| `anthropic-messages` | *required* | `POST /v1/chat/completions` (OpenAI and every OpenAI-compatible server) or `POST /v1/messages` (Anthropic). |
| `base_url` | URL | *required* | The server, `http://` or `https://`, with or without a trailing `/v1`; evald adds `/v1/…` itself. A path before `/v1` is kept (a gateway mounting the API under a prefix). A `user:password@` in it is refused: the URL is stored and printed in the clear. |
| `api_key_env` | string | unset | The environment variable holding the key, read at call time only. Unset: the endpoint takes no key (a local server). Keys never go in `headers:`, which are written in the file. |
| `ca_file` | path | unset | PEM CA certificates to trust, for a server behind a private CA. Relative paths resolve against the config file's directory. |
| `client_cert`, `client_key` | path | unset | mTLS: a PEM client certificate and its private key, both or neither. |
| `timeout_secs` | integer ≥ 1 | `120` | Per-call timeout. |
| `max_in_flight` | integer ≥ 1 | `1` | Judge calls to this endpoint in flight at once. Keep `1` for a server that also serves production. |
| `headers` | map | `{}` | Extra request headers (a gateway's routing or team header). Part of a cached score's key: a header can route the same URL to another backend. |
| `structured_output` | `auto` \| `json_schema` \| `json_object` \| `none` | `auto` | How the reply is constrained to the score JSON (`openai-chat` only; the Messages API has no `response_format`). `auto` asks for a JSON schema and steps down to `json_object`, then to none, when the server refuses it. A fixed mode is sent as written. |
| `jurisdiction` | string | unset | Your label for where this endpoint's data goes (`MY`, `EU`, `on-prem`). Recorded on every call in the egress ledger and in each run's manifest; evald does not check it. |
| `retries` | integer 0–10 | `3` | Retries of a call that got a 408, a 429, a 500/502/503/504, a 529 (Anthropic's "overloaded") or no connection. A `Retry-After` of whole seconds is honoured (at most 30 s); otherwise 0.5 s, 1 s, 2 s, … up to 8 s. |

The implicit endpoints that `provider:` names come from the environment (read **at call time
only**, never logged or persisted):

| Env var | Used when | What it does |
|---|---|---|
| `ANTHROPIC_API_KEY` | `provider: anthropic` | BYO key. Required for real calls (not for `--estimate`). |
| `OPENAI_API_KEY` | `provider: openai` | BYO key. |
| `ANTHROPIC_BASE_URL` | `provider: anthropic` | Override the API base (default `https://api.anthropic.com`), with or without `/v1`. |
| `OPENAI_BASE_URL` | `provider: openai` | Override the API base (default `https://api.openai.com`), with or without `/v1` (the OpenAI SDKs' own convention used to become `/v1/v1`). |

Judge results are cached in `<data-dir>/judge_cache.redb` keyed by
`(version, api, base URL, rail, model, criteria, item)` — re-running an unchanged eval is free;
the cache stores only scores + reasoning, never inputs or keys. Each score also records the
model id the endpoint listed for the model (`GET /v1/models`; vLLM's `root`, the weights it
loaded), and a hit is served only while the endpoint still lists the same id: one server's
cache never answers for another, or for other weights swapped in behind the same name. A
server without a model list is identified by its URL alone. An endpoint's calls run up to its
`max_in_flight` at once, and the results keep dataset order.

**What a judge call sends, and what happens when a server refuses part of it.** Every call asks
for `temperature: 0`; an `openai-chat` call also sends `seed: 0` and, under `auto`, a strict
JSON schema for `{score, reasoning}` (`response_format: json_schema`). Small and local servers
differ in what they accept, so a `400`/`422` whose message names one of these is answered by
sending the call again without it, one parameter at a time: the temperature (a reasoning model
refuses it), then the seed, then the schema (to `json_object`, then to no `response_format` at
all). What an endpoint refused is remembered for the rest of the run, so the next item does not
ask again. A refusal that names none of them fails the call with the server's message. The reply
is parsed tolerantly either way: a JSON object wrapped in prose or code fences, after a reasoning
model's `<think>…</think>`, with the score clamped to `[0, 1]`.

**What a run records about its judges.** Each judge call made (not cache hits) is written to the
store as a span with `service.name = evald.judge`, one trace per run: `chat <model>`, its
duration, the model the server says answered, tokens, the attempts it took and the
`response_format` it ended with; a reply with no usable score is an error span. They are model
calls like any other, so the judge's own spend and latency show in `evald cost` and
`evald latency` (`--by service` puts them on their own row). Every run also
writes a manifest, a score named `evald.run.manifest` on the run, whose JSON names each judge's
endpoint, base URL, the model id the endpoint listed (`server_model`), the models that answered
(`answered_as`), which weights those were (`provenance`, [below](#which-weights-answered)), the
structured-output mode, calls, cache hits, tokens and the judge's version:

```bash
evald query "SELECT name, model, prompt_tokens FROM spans WHERE service_name = 'evald.judge'"
evald query "SELECT str_value FROM scores WHERE name = 'evald.run.manifest'"
```

### Which weights answered

A model name is not a model: an Ollama tag can be pulled again over new weights, and a gateway
can route one name to another deployment. So each judge's **provenance** is read from its
endpoint once per run and recorded in the manifest (`provenance`) and on every judge span
(`evald.model.*`, until the GenAI semantic conventions have keys for it):

| Field | Span attribute | From |
|---|---|---|
| `id` | `evald.model.id` | `GET /v1/models`: vLLM's `root` (the path or Hugging Face id of the weights it loaded), else the listed id. |
| `digest` | `evald.model.digest` | Ollama's `GET /api/tags`: the content digest of the weights behind the tag. |
| `quantization` | `evald.model.quantization` | Ollama: `details.quantization_level` (`Q4_K_M`, `Q8_0`, `F16`). |
| `parameter_size` | `evald.model.parameter_size` | Ollama: `details.parameter_size` (`7.6B`). |
| `family` | — | Ollama: `details.family`. |
| `source` | — | `v1/models`, or `v1/models+ollama` when Ollama added a digest. |

`/api/tags` is asked only when the model list looks like Ollama's: the model listed as its
`name:tag` (a bare name as `name:latest`) with no `root`. vLLM's entry already names the weights
and a hosted API's ids carry no tag, so neither is sent the extra request; a server that is not
Ollama after all answers it 404 and the provenance stays what the list said. The request goes through the [egress door](#outbound-calls---offline---egress-allow) like
every other. vLLM reports no Hugging Face revision: its `root` is whatever its `--model` was,
so serve from a snapshot directory (`…/snapshots/<commit>`) when the revision itself must be
recorded.

**The judge cache keys on it.** A cached judge score is reused only for the same weights: the
digest when there is one, else the listed id. Pull new weights under the same tag and the next
run asks the judge again instead of serving the old weights' scores. Online evals
(`serve --online-eval`) read provenance once when `serve` starts; restart it after re-pulling a
judge model.

```bash
evald query "SELECT raw_attributes_json FROM spans WHERE service_name = 'evald.judge' LIMIT 1"
```

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
