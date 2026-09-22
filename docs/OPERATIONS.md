# evald operations runbook

Deploying, backing up, upgrading, monitoring, and debugging a running evald.
evald is a **single static binary + one local `--data-dir`** — there is no
database, container, or sidecar to operate. Durability contract, stated
honestly: **accepted spans are durable (fsynced before the ACK); under overload
evald sheds with explicit `429 + Retry-After` — it never silently drops.**
Internals (commit protocol, data model) live in [`PLAN.md`](../PLAN.md);
measured capacity numbers live in [`BENCHMARKS.md`](./BENCHMARKS.md).

## What the state is — `--data-dir` layout

Everything evald persists lives under one directory (default `evald-data`,
see [CONFIG.md](./CONFIG.md)):

```text
evald-data/
├── FORMAT                    # on-disk format version + the evald that created the dir
├── wal/                      # segmented write-ahead log — the ACK boundary
│   └── 00000000000000000042.wal    #   [u32 len][u32 crc32][JSON] frames; sealed segs await compaction
├── blocks/                   # cold tier — plain Snappy Parquet, time-partitioned
│   ├── 2026/07/07/17/00000000000000000041-0.parquet   #   one sealed WAL segment
│   ├── 2026/07/07/17/merged-…-….parquet               #   an hour's blocks, merged
│   └── 2026/07/07/day-…-….parquet                     #   a closed day, merged
├── index.redb                # trace_id → block index + the compaction watermark
├── scores.redb               # the universal Score store (evals, human annotations, API)
└── judge_cache.redb          # (only with LLM-judge use) cached judge results — scores only, never inputs/keys
```

The blocks are an open format — DuckDB and pandas read them directly. The full
file-by-file contract, including what each block's Parquet metadata records and
the compatibility promise across releases, is [FORMAT.md](./FORMAT.md).

The write path: OTLP request → normalize → bounded channel → single writer task
appends + fsyncs the WAL (**then** the client is ACKed) → hot tier (in memory)
→ background compactor flushes sealed segments to Parquet, records blocks +
advances the watermark in one redb transaction, then deletes the WAL segment.
Reads always see hot ∪ cold, deduped.

The same compactor also merges cold blocks into fewer, larger ones, so the block
count — and with it the number of files a query opens — stays bounded instead of
growing with every sealed segment. A merge is committed exactly like a flush
(write, fsync, rename, then one redb transaction), and its inputs are unlinked
only after a grace window, so a query already reading them is never pulled out
from under. `evald compact` runs the same pass on demand. See
[FORMAT.md § Merged blocks](./FORMAT.md#merged-blocks) and the
`--cold-merge-*` flags in [CONFIG.md](./CONFIG.md).

### Crash recovery semantics

On every open (`serve`, `eval`, `query`, `cost` all open the same store):

1. WAL segments **at or below** the index watermark are already in cold —
   deleted.
2. Segments **above** the watermark are replayed into the hot tier — nothing
   ACKed is lost, nothing is double-counted (the watermark only advances after a
   block is fsynced + renamed into place).
3. Orphan Parquet files not referenced by the index (a flush that crashed before
   its redb commit) and leftover `*.tmp` are swept — the index is the source of
   truth for committed blocks.

This is exercised by a real `kill -9` during compaction in
[`tests/`](../tests) and was re-verified by hand (kill the server mid-run;
reopen; the spans come back). A torn final WAL frame (crash mid-append) stops
replay at the last intact record — that final, never-ACKed batch is the only
thing a crash can lose.

## Deploy

Copy one binary; give it a directory:

```bash
evald serve --data-dir /var/lib/evald            # loopback :4318
```

- Run it under any supervisor (systemd, container, CI step). Ctrl-C / SIGINT
  triggers a graceful drain.
- **One process per data-dir.** The redb lock is exclusive — a second process
  (including `evald eval run` / `query` / `cost`) opening the same dir fails
  with `Database already open. Cannot acquire lock.` Point CLI runs at their own
  `--data-dir`, or stop the server first.
- Container images: see [`docs/DOCKERHUB.md`](./DOCKERHUB.md) and the
  [`Dockerfile`](../Dockerfile)s.

## Backup & restore

The state to snapshot is the **whole `--data-dir`** (WAL + blocks + the redb
files together — they are one consistent unit tied by the watermark).

Safe backup, in preference order:

1. **Cold backup** (simplest, always consistent): stop the process, copy the
   directory, restart.
2. **Filesystem/volume snapshot** (LVM, ZFS, EBS) while running: the commit
   protocol is crash-safe, so restoring a point-in-time snapshot behaves exactly
   like recovering from a power cut — ACKed spans up to the snapshot are
   present.
3. **Do not** `rsync`/copy the live directory file-by-file while the server is
   writing: the WAL, blocks, and index would be captured at different moments;
   an index that references a block the copy missed makes reads on the restored
   dir fail.

Restore = put the directory back and start evald against it. Recovery runs
automatically on open.

### The drill, and the measured numbers

The procedure above is **drilled on every push** (`ops/restore-drill.sh`, run by
the `restore-drill` CI job) — run it yourself against your own data the same way:

```bash
ops/restore-drill.sh target/release/evald
```

It ingests, takes a cold backup, keeps serving, destroys the data-dir, restores,
and asserts the property that matters:

> **A restore loses nothing that was ACK'd before the backup point.**

That is the durability claim — the WAL fsync is the ACK boundary — extended
across a backup. Loss bounded by backup age is an RPO window; loss *inside* the
backup point is a bug. The drill distinguishes them and fails on the second.

It is verified to fail, not merely to pass: backing up `blocks/` and the redb
files but omitting `wal/` — the partial-copy mistake warned against above — drops
spans that were ACK'd before the backup, and the drill exits non-zero naming the
count.

Measured on one box (2,500 spans, 2.2 MB store, debug binary):

| | |
|---|---|
| **RPO** | **= backup age.** Nothing ACK'd before the backup was lost; the only loss was the 500 spans ACK'd after it. Snapshot more often to shrink it — there is no additional loss to account for. |
| **RTO** | **0.54 s** — copy back, open, WAL replay, to `/readyz`. Dominated by store size, so scale it from your own data-dir: this is ~0.5 s for 2.2 MB on a warm page cache, and a release binary recovers faster than the debug one measured here. |

Re-run the drill after any change to how you take backups. The numbers above are
from the harness, not an estimate — if yours differ, yours are the true ones.

Partial-loss note: the Parquet blocks are plain files — even with a damaged
`index.redb`, the span **data** in `blocks/` remains readable by DuckDB/pandas.
Scores live only in `scores.redb`.

## Upgrade

The on-disk format is versioned and frozen at **format 1**, and a release reads
its own format and the one before it — so an upgrade is a binary swap, never an
export and re-import. The full policy is [FORMAT.md](./FORMAT.md).

1. Stop evald (graceful — lets in-flight WAL appends finish).
2. Back up the data-dir (above).
3. Swap the binary and start it. Check the startup line
   (`store opened spans_recovered=… watermark=… format=1`) plus a
   `GET /v1/spans` smoke read.
4. Rollback = restore the old binary + the backup.

Two things the new binary may tell you instead:

- **A data-dir written before the freeze** is stamped with its format marker on
  first open. Nothing is rewritten and no data moves — only the statement of the
  format was missing. `evald migrate --data-dir … --dry-run` reports it in
  advance; without `--dry-run` it does the stamping outside a service window.
- **A data-dir written by a NEWER evald** is refused, naming both versions.
  That is a downgrade, and the fix is to put the newer binary back — restoring
  the backup from step 2 is the only supported way to go backwards.

## Retention & disk guardrails

### Retention

Cold Parquet blocks are reclaimed by age. There is **no artificial cap** on how
much a local node may keep, and **no automatic deletion unless you ask for it**.

Run it continuously on the server:

```bash
evald serve --retention 30d --retention-interval-secs 3600
```

Unset, nothing is ever deleted. A block is dropped only when **every** span in it
predates the window: the index entry goes first, then the file is unlinked, so a
crash mid-sweep leaves an orphan the next open collects. Reclamation is
`O(unlink)` — no row-by-row delete, no vacuum, no full-table scan.

Or sweep once, by hand:

```bash
evald retention --older-than 30d --data-dir /var/lib/evald --dry-run   # preview
evald retention --older-than 30d --data-dir /var/lib/evald             # reclaim
```

A block is dropped only when **every** span in it predates the window, so the
sweep never takes a partially-live partition. Units: `d`/`h`/`m`/`s`, or a bare
number for seconds. Always `--dry-run` first — it reports exactly what would be
reclaimed and deletes nothing.

Watch `evald_retention_blocks_dropped_total` and
`evald_retention_bytes_reclaimed_total` to see what the sweep is actually
reclaiming; if they stay flat while the disk fills, the window is too wide.

### Disk guardrails

Retention bounds growth over time; the guardrail is the safety valve for when it
is not enough — or when something *else* fills the volume.

| Flag | Default | Effect |
|---|---|---|
| `--disk-warn-free` | `1GiB` | log a warning, keep accepting ingest |
| `--disk-min-free` | `256MiB` | **refuse ingest** with `503 + Retry-After: 30` |
| `--disk-check-interval-secs` | `10` | how often free space is sampled (`0` = guardrail off) |

The floor refuses cleanly *before* a write hits `ENOSPC` part-way through a WAL
segment or a Parquet flush, which is the failure that wedges a store. It is
checked before the **blob offload** as well as before the WAL append: the offload
writes, so a refusal that came only at the append would spend the last of a full
volume writing blob files and then reject the span that was their only reference,
leaving them behind with nothing that ever collects them. It is
`503`, not the `429` a backlog shed returns: both are retryable to an OTLP
exporter, but `429` means "you are sending too fast" and a full disk is neither
the client's fault nor within its power to fix. Keeping them distinct means
`evald_spans_shed_total` and `evald_spans_disk_blocked_total` answer two
different questions.

Free space is measured as space available to an **unprivileged** process
(`statvfs` `f_bavail`), so the filesystem's root reserve is not counted as room
evald can use. Sampling is on a timer, so an append pays one atomic read.

**The guardrail fails open.** If free space cannot be read — a non-Unix target,
or a failing probe — it never blocks ingest and logs that it is inactive.
Refusing writes because the disk could not be *measured* would invent an outage.

To recover from the floor: free space (or let retention run), and the next sample
clears the block — `disk guardrail: recovered` appears in the log and exporters'
retries land.

## Redaction

Off by default. Turn it on to strip sensitive values from prompts, completions
and span attributes **before anything is written**:

```bash
evald serve --redact all --redact-action hash \
            --redact-custom 'employee_id=EMP-[0-9]{6}'
```

Classes: `email`, `credit_card`, `ssn`, `phone`, `ip`, `api_key`, `jwt`, plus any
`--redact-custom` rules.

### What the guarantee is

Redaction runs **before the WAL append and before the blob offload**. The WAL is
the ACK boundary, so anything reaching it is durable by definition; a detected
value therefore never touches disk in *any* tier — not the WAL, not a blob, not a
Parquet block. This is enforced by a test that walks every byte of the data-dir
after an ingest and asserts the raw values are absent.

The corollary: **it is irreversible.** There is no "unredact" — the original is
gone before anything is written. That is the property a regulated buyer is
actually buying, and the reason it is opt-in.

### Precision

A redactor that mangles ordinary text gets switched off, and then protects
nothing. So the detectors prefer a missed match to a false one:

- **Credit cards are Luhn-checked.** A bare 13–19 digit regex matches order
  numbers, trace ids and timestamps; the checksum removes essentially all of
  them. `card 4242424242424242` is redacted; `ref 1234567812345678` beside it is
  not.
- **API keys match published vendor shapes** (`sk-`, `AKIA`, `ghp_`, `xox…`), not
  "a long random-looking string" — which would hit every trace id you store.
- **`ip` is the exception.** `1.2.3.4` is both a routable address and a plausible
  version string, and no validation separates them. Enable it when addresses are
  PII in your jurisdiction (they are under GDPR) and expect version strings to be
  caught with them. This is why classes are selected individually rather than
  only as `all`.

### Actions

| Action | Result | Use when |
|---|---|---|
| `redact` (default) | `[REDACTED:email]` | you want the trace readable and the removal obvious |
| `hash` | `[email:9f2a…]` | you still need to group or join on the value. Equal values hash equally; the digest is salt-free so it stays comparable across restarts and fleet nodes. Treat it as a **pseudonym, not anonymisation** — a small value space (an SSN) is brute-forceable from a digest |
| `drop` | removed entirely | the value should leave no trace at all |

### What is deliberately NOT scanned

`user_id`, `session_id`, `service_name` and the span name. These are identifiers
the caller chose to send, are usually already opaque, and rewriting them would
silently break `evald cost --by user|session`. **If you put an email in
`user_id`, hash it upstream** — evald will not do it for you.

Attribute *keys* are also left alone: a key is a schema name your
instrumentation chose, and rewriting keys would break every query written
against them.

### Verifying it

`evald_redactions_total{rule="…"}` counts what was rewritten, per rule. It counts
**occurrences in the stored representation**, not distinct values: `normalize`
promotes `input.value`/`output.value` into their own fields while also preserving
them in `raw_attributes`, so one email in a prompt is rewritten — and counted —
in both copies. A rule that is armed but never fires still publishes a `0`, so
`rate()` has a baseline.

## Monitoring

evald exposes **Prometheus metrics** at `GET /metrics` (text exposition format
0.0.4) and two Kubernetes probes, `GET /healthz` and `GET /readyz`. It also logs
structured `tracing` lines to stderr.

### Logs

Logs go to **stderr** (so a command's real output stays parseable on stdout) and are
filtered by **`RUST_LOG`**, which defaults to `info`.

At `info` — the default — ingest costs **one line per request**, not one per span:

```text
INFO evald::ingest: ingesting OTLP/HTTP traces resource_spans=1 spans=10
```

The per-span detail line — dialect, trace/span id, name, `oi_kind`, model, provider,
token counts, `cost_usd`, duration, attribute count — is at **`debug`**. Turn it on
when you are inspecting what an SDK actually sends:

```bash
RUST_LOG=evald=debug evald serve          # per-span lines back, everything else too
RUST_LOG=evald::ingest=debug evald serve  # just the ingest detail
```

Do not leave it on under load. Formatting and ANSI-colouring those ~14 fields measured
**~38,000 instructions per span — about a third of the whole ingest hot path** (3,000
spans over OTLP/HTTP under callgrind: ~113k instructions/span at `debug` vs ~75k at
`warn`, compaction off). End to end, on a 4-vCPU VM whose ceiling is fsync rather than
CPU, turning it on still cost **+14% of the server's CPU per span and −7% throughput**
([BENCHMARKS.md § Log level](./BENCHMARKS.md#log-level--read-before-comparing-any-of-these-numbers)).
It is at `debug` for exactly that reason.

Going the other way costs nothing: the default and `RUST_LOG=warn` measured within 0.5%
of each other, so you do not need to silence evald to benchmark it — though `warn` is
still the right setting for an apples-to-apples comparison against published numbers.

`RUST_LOG` takes the full [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
syntax, so per-module levels compose: `RUST_LOG=warn,evald::store=info` keeps
compaction and recovery lines while silencing the rest.

### Probes

| Endpoint | Answers | Behaviour |
|---|---|---|
| `/healthz` | liveness — is this process still a working server? | always `200 ok` while it serves. It deliberately does **not** consult the store: a liveness probe that fails on a slow disk or a wedged compactor makes the kubelet kill a process that still holds a good WAL, turning a degradation into a restart loop. |
| `/readyz` | readiness — should traffic come here? | `200 ready`, or `503` once the store's writer task is gone (nothing can be made durable). **Shedding is not a readiness failure** — it is the documented backpressure contract (`429 + Retry-After`), so a node under load stays ready and keeps telling clients to slow down. |

Both are **exempt from the bearer-token gate** (a kubelet sends no
`Authorization` header), and neither reveals anything a port scan would not.
`/metrics` *is* behind the gate when it is armed — a scrape exposes ingest rates
and backlog depth. Prometheus supports that natively via `authorization:` /
`bearer_token_file:` in `scrape_configs`.

```yaml
# k8s probes
livenessProbe:
  httpGet: { path: /healthz, port: 4318 }
readinessProbe:
  httpGet: { path: /readyz, port: 4318 }
```

### Metrics

| Series | Type | What it tells you |
|---|---|---|
| `evald_build_info{version}` | gauge | always `1`; the running version, for dashboard joins |
| `evald_spans_ingested_total` | counter | spans **durably ACK'd** (counted at the committing fsync). `rate()` it for ingest/s |
| `evald_spans_shed_total` | counter | spans shed by backpressure (answered `429`) |
| `evald_hot_spans` | gauge | un-compacted backlog the compactor must drain |
| `evald_hot_spans_max` | gauge | the bound shedding starts at (`0` = unbounded) |
| `evald_ingest_shedding` | gauge | `1` while shedding |
| `evald_ingest_channel_capacity` | gauge | in-flight appends before a channel-full shed |
| `evald_compactions_total` | counter | background compaction passes completed |
| `evald_compaction_failures_total` | counter | background compaction passes that failed |
| `evald_wal_bytes` | gauge | bytes in the WAL — the compactor's backlog in bytes |
| `evald_eval_events_ingested_total` | counter | `gen_ai.evaluation.result` span events stored as scores |
| `evald_eval_events_malformed_total` | counter | such events dropped as malformed (the spans in the same request are unaffected) |
| `evald_disk_free_bytes` | gauge | free bytes at the guardrail's last sample. **Absent** when the guardrail is off or the probe failed — deliberately, since a `0` would read as "disk full" to every alert |
| `evald_disk_blocked` | gauge | `1` while ingest is refused by the floor |
| `evald_spans_disk_blocked_total` | counter | spans refused by the floor (distinct from `evald_spans_shed_total`) |
| `evald_retention_sweeps_total` | counter | automatic retention sweeps completed |
| `evald_retention_blocks_dropped_total` | counter | blocks dropped by automatic retention |
| `evald_retention_bytes_reclaimed_total` | counter | bytes reclaimed by automatic retention |

Whole-store inventory (total spans, total scores) is **deliberately absent**:
both require a full scan, and paying for one every 15 seconds turns the
monitoring endpoint into the outage. Query those through `/v1/sql`, where the
cost is the caller's choice. Filesystem free space is likewise absent — that is
the node exporter's job, and it already knows the mount points.

Alerts worth having:

| Signal | Expression | Why |
|---|---|---|
| sustained shedding | `evald_ingest_shedding == 1` for 5m | ingest is outrunning WAL fsync throughput |
| compactor wedged | `rate(evald_compaction_failures_total[10m]) > 0`, or `evald_hot_spans` climbing while `rate(evald_compactions_total[10m]) == 0` | the hot tier will grow until ingest sheds |
| WAL growth | `evald_wal_bytes` growing monotonically | compaction lagging or disabled (`--compact-interval-secs 0`) |
| ingest stalled | `rate(evald_spans_ingested_total[5m]) == 0` when traffic is expected | exporters mis-pointed, or the gate is rejecting them |
| store write failures | log `store write failed` (clients see 503) | usually disk full / permissions |
| recovery cost | startup log `spans_recovered=N` | large N with slow starts — lower `--seal-threshold` |
| **out of disk** | `evald_disk_blocked == 1` | ingest is being refused — free space or widen retention. Page on this |
| disk running low | `evald_disk_free_bytes < 2 * <your --disk-min-free>` | the floor is approaching; act before it bites |
| retention not keeping up | `rate(evald_retention_blocks_dropped_total[1h]) == 0` while `evald_disk_free_bytes` falls | the window is too wide for the ingest rate |

### LLM usage: cost, tokens, latency, quality

Besides its own health, `/metrics` carries the usage of the LLM calls evald has accepted: cost, input /
output / cached / reasoning tokens, requests and errors, a latency histogram, a time-to-first-token
histogram where spans carry one, and the rolling mean of each evaluator's scores. The full series list,
labels and caps are in [API.md](./API.md#llm-usage-series); the labels are `gen_ai_provider_name`,
`gen_ai_request_model` and `service_name` only, so the series count stays small (100 models by default,
`--metrics-model-cap`) and never grows with users or sessions. `--no-usage-metrics` removes them.

Counters are since process start and approximate under exporter retries (a re-sent batch is counted
twice); the SQL recipes below are exact. Alert on them where you already alert, next to the inference
server's own metrics. Example Prometheus rules:

```yaml
groups:
  - name: evald-llm
    rules:
      - alert: LLMSpendRateHigh            # dollars per hour, across all models
        expr: sum(rate(evald_llm_cost_usd_total[1h])) * 3600 > 25
        for: 15m
      - alert: LLMCostIsPartial            # cost is missing on >5% of spans: the alert above understates
        expr: sum(rate(evald_llm_spans_without_cost_total[15m])) / sum(rate(evald_llm_requests_total[15m])) > 0.05
        for: 30m
      - alert: LLMLatencyP95High
        expr: histogram_quantile(0.95, sum by (le, gen_ai_request_model) (rate(gen_ai_client_operation_duration_seconds_bucket[10m]))) > 10
        for: 10m
      - alert: LLMErrorRateHigh
        expr: sum(rate(evald_llm_request_errors_total[5m])) / sum(rate(evald_llm_requests_total[5m])) > 0.05
        for: 10m
      - alert: LLMQualityDropped           # a named evaluator's recent mean fell
        expr: evald_eval_score_mean{evaluator="faithfulness"} < 0.8
        for: 15m
      - alert: LLMLabelsFolded             # more distinct models than --metrics-model-cap
        expr: increase(evald_usage_labels_folded_total[1h]) > 0
```

**Exact latency percentiles.** `evald latency [--by model|provider|service]` prints nearest-rank p50 / p95 /
p99 and the max of `end − start` over LLM spans, plus, for the spans that carry a time to first token, how
many do, how many do not (`unknown`; never estimated) and their p50 / p95. Nearest-rank means the p-th
percentile of `n` sorted values is the value at rank `ceil(p·n)`: always a span that really happened,
never an interpolation. The same thing in SQL, over `POST /v1/sql` or `evald query`:

```sql
WITH d AS (SELECT model AS g, end_unix_nano - start_unix_nano AS dur FROM spans WHERE model IS NOT NULL),
r AS (SELECT g, dur, ROW_NUMBER() OVER (PARTITION BY g ORDER BY dur) AS rn,
             COUNT(*) OVER (PARTITION BY g) AS n FROM d)
SELECT g AS model, MAX(n) AS spans,
       MIN(CASE WHEN rn >= CEIL(0.50 * n) THEN dur END) / 1e6 AS p50_ms,
       MIN(CASE WHEN rn >= CEIL(0.95 * n) THEN dur END) / 1e6 AS p95_ms,
       MIN(CASE WHEN rn >= CEIL(0.99 * n) THEN dur END) / 1e6 AS p99_ms
FROM r GROUP BY g ORDER BY spans DESC
```

**Overhead.** The series are recorded by the writer after the commit fsync, taking one lock per committed
batch (not per span); label values are interned once, so a span costs a few integer hash lookups and
additions and allocates nothing. A cap-hit scrape (100 models) is about 28 lines and 4–5 KB per series.

Capacity guidance (measured, same-box: see [BENCHMARKS.md](./BENCHMARKS.md)):
~56k spans/s fsynced-before-ACK at 32 connections on ~1.8 cores, 8.5 MB idle
RSS. SQL scans run off the ingest hot path but do burn CPU — schedule heavy
analytics accordingly.

## Model price table: build, load, refresh

Most instrumentors report a model and token counts but no cost. evald prices those spans **at ingest**
from a price table and stores the result in `cost_usd`; a span that already carries its own cost is never
touched, and a model the table does not know is left without a cost (`(no price)` in `evald cost`), never
priced at `$0`. There is no network code in this path: **you** keep the table fresh (see *Refreshing*).

### What is built in

A baseline (a trimmed copy of a public price list, pinned to a commit and date) is compiled into the binary,
so a fresh install prices the common models with no configuration. Check which version is in use on
`GET /v1/meta` (`price_table`); every Parquet block also names the table in force when it was written
(`evald.price_tables` in its footer metadata), and the version is in the `price table reloaded` log line
and in the staleness warning. Nothing per span records it: a priced span carries its `cost_usd` and nothing
else, and `evald cost --price-table` re-prices from the stored token counts whenever the table moves.

### The file format

The format is the public LiteLLM `model_prices_and_context_window.json` shape: one JSON object, model id
to prices. Costs are **USD per token** (a $3 per million-token rate is `0.000003`). Only the fields evald
prices with are read; everything else in an entry is ignored, so the upstream file works unchanged.

| Field | Meaning |
|---|---|
| `input_cost_per_token`, `output_cost_per_token` | Required (at least one). An entry with neither is skipped |
| `cache_read_input_token_cost`, `cache_creation_input_token_cost` | Cached-input rates. Absent = the input rate (no discount is invented) |
| `output_cost_per_reasoning_token` | Reasoning tokens, when priced differently from output |
| `<field>_above_200k_tokens` (and other `_above_<N>k_tokens`) | Replaces that rate once a request's input passes N tokens |
| `_evald: {"commit": "...", "date": "YYYY-MM-DD"}` | Optional. Names the source: the version becomes `<commit8>@<date>` and the date drives the staleness warning. Without it the version is `sha256:<8 hex>` and the file's modification time is its date |

Model ids are matched case-insensitively, after dropping provider prefixes (`anthropic/…`, `bedrock/…`), a
Bedrock region prefix, an `@date` and trailing snapshot segments (`-20250929`), so
`claude-sonnet-4-5-20250929` finds `claude-sonnet-4-5`. A more specific name never falls back to a
different model (`gpt-4o-mini` does not price as `gpt-4o`).

### Building your table

**Recommended: a small overlay** with just your own models and any corrections. The file is laid over the
built-in table and wins for every model it names; everything else still comes from the built-in one.

```json
{
  "_evald": { "commit": "internal", "date": "2026-09-22" },
  "acme-support-ft-v3": {
    "input_cost_per_token": 0.000003,
    "output_cost_per_token": 0.000015,
    "cache_read_input_token_cost": 0.0000003
  },
  "llama-3.1-70b-instruct-local": {
    "input_cost_per_token": 0.0000002,
    "output_cost_per_token": 0.0000002
  }
}
```

The second entry shows a self-hosted model: pick a per-token figure that reflects your amortised GPU cost
(evald cannot know it), so cost by model stays comparable with hosted models.

**Or take the upstream file as it is.** It is accepted unchanged (`sample_spec` and entries without a
per-token price are skipped), for example
`curl -fsSL https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json -o prices.json`.
It is large; an overlay is easier to review. The overlay's version is stamped as `<file version>+<built-in
version>`, so a stored cost tells you both.

To rebuild the *built-in* baseline (for building evald itself, not for operating it) use
`scripts/gen_price_baseline.py`; it records the upstream commit and date under `_evald`.

### Loading it

```
evald serve --price-table /etc/evald/prices.json        # or EVALD_PRICE_TABLE=/etc/evald/prices.json
evald cost  --price-table /etc/evald/prices.json        # re-price a report (see below)
```

- **Containers.** Mount the file (`-v /etc/evald/prices.json:/etc/evald/prices.json:ro`).
- **Kubernetes.** Mount a ConfigMap or Secret as a file and point `--price-table` at it. Do **not** use
  `subPath`: a subPath mount never updates, so hot reload cannot see the change.
- **Start-up.** A file that does not parse stops `serve` with the error, rather than silently pricing with
  the wrong table. A table whose date is more than 90 days old logs a warning: prices move.

### Refreshing it (hot reload)

`serve` checks the file's modification time at most every 2 seconds. On a change it parses the new file off
to the side and swaps it in atomically:

- **Success:** `price table reloaded` is logged with the new version and model count. New spans are stamped
  with the new version; spans already stored keep theirs.
- **Failure** (bad JSON, unreadable): `price table reload failed; keeping the previous table` is logged
  **once per edit**, and ingest carries on with the last good table. It never runs without a table.

Write the file atomically so a half-written file is never read: write next to it and rename.

```sh
# refresh from your own mirror of the price list, only if it parses
curl -fsSL "$MIRROR/model_prices_and_context_window.json" -o /etc/evald/prices.json.new \
  && jq -e 'type == "object"' /etc/evald/prices.json.new >/dev/null \
  && mv /etc/evald/prices.json.new /etc/evald/prices.json
```

In an air-gapped site, ship the file with your configuration management or a scheduled job on a connected
host, and copy it in. The mechanism is the same: replace the file, evald notices.

### Checking that it works

```sh
curl -s localhost:4318/v1/traces -H 'content-type: application/json' -d '{"resourceSpans":[{"scopeSpans":[{"spans":[
  {"traceId":"0123456789abcdef0123456789abcdef","spanId":"00000000000000a1","name":"t",
   "startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000500000000",
   "attributes":[{"key":"gen_ai.request.model","value":{"stringValue":"acme-support-ft-v3"}},
                 {"key":"gen_ai.usage.input_tokens","value":{"intValue":"1000"}},
                 {"key":"gen_ai.usage.output_tokens","value":{"intValue":"200"}}]}]}]}]}'
curl -s localhost:4318/v1/spans | jq '.[] | {cost_usd}'
curl -s localhost:4318/v1/meta | jq .price_table
```

With the example overlay the cost must be `0.006` (1000 × 0.000003 + 200 × 0.000015) and `/v1/meta` must
name your file's version (`<yours>+<baseline>`). Work the expected number out by hand before trusting the
table.

### Correcting past data

Stored costs are never rewritten. To see what a different table would have said:
`evald cost --price-table new.json` recomputes the report from the stored token counts (spans whose cost the
application reported keep it). Fixing a price going forward is just a file edit; fixing history is a report.

### When a cost is missing or looks wrong

| Symptom | Cause | Fix |
|---|---|---|
| `(no price)` for a model | the model id is not in the table (or matched nothing after normalisation) | add it to your overlay; check the exact id on the span |
| No cost on a span at all | it has no model, or no token counts, or the app supplied its own cost | instrument the missing attribute; a supplied cost is kept by design |
| Cost too high or low on cached prompts | the source counts cached tokens differently | see the per-source table in [INSTRUMENTATION.md](./INSTRUMENTATION.md); report a total token count so evald can tell |
| A table edit is not picked up | a `subPath` mount, an unchanged modification time, or invalid JSON (check the log for the once-per-edit warning) | remount without `subPath`; write to a temp file and rename |
| Stale-table warning | the table's `_evald.date` (or file date) is over 90 days old | refresh the file |
| A lot of distinct model ids | one series per model on `/metrics` | `--metrics-model-cap` folds the rest into `other` |

## Troubleshooting (symptom → cause → fix)

**`429 Too Many Requests` + `Retry-After: 1` (`evald: overloaded, retry shortly`)**
The bounded ingest channel (1024 batches) is full — WAL fsync throughput can't
keep up with the arrival rate. This is deliberate shedding, not data loss:
un-ACKed batches were never accepted. Fix: let the SDK's OTLP exporter retry
(it honors Retry-After), batch spans (100-span batches nearly double measured
throughput), put the data-dir on faster storage, or slow the producer.

**Queries fail with `Too many open files` / `EMFILE`**
`SELECT`s against `/v1/sql` (or `evald query`) error out while the server keeps
accepting writes happily. **Your data is intact**; it has become unreadable, not
lost.

This was the shape of the cold read path before two fixes, and on a current build
it should not happen: a scan now holds at most 64 blocks open at a time whatever
the block count, and the compactor merges cold blocks so the count stays bounded.
If you see it anyway:

1. Check `evald_cold_blocks` on `/metrics`. If it is large and
   `evald_cold_merges_total` is flat, merging is off or failing — look for
   `cold merge failed` in the logs and check `evald_cold_merge_failures_total`.
   `--cold-merge-threshold 0` disables merging; the default is `16`.
2. Run `evald compact --data-dir …` against a stopped node to collapse the
   backlog in one pass (`--dry-run` first to see what it would do).
3. Raise the process descriptor limit (`LimitNOFILE=` in the systemd unit,
   `ulimit -n` otherwise) as immediate headroom.
4. Reduce how fast blocks are made: raise `--seal-threshold` (default `50000`)
   so each block holds more spans, and set a `--retention` window so old
   partitions are dropped rather than accumulating forever.

**`413 Payload Too Large` on `POST /v1/traces`**
One OTLP batch decompressed past 16 MiB. Lower the SDK's
`max_export_batch_size`. The cap is on the *inflated* size, so a tiny gzip that
expands past the cap is also 413 — that is the decompression-bomb guard working.

**`503 Service Unavailable` (`evald: store unavailable`)**
The writer task could not append/fsync — almost always disk full, a permissions
change, or the data-dir volume disappearing. Check stderr for
`store write failed` and the underlying io error; free space and restart.

**`Error: opening store at …: Database already open. Cannot acquire lock.`**
Two processes on one data-dir (e.g. `evald eval run` while `evald serve` holds
the same `--data-dir`). Stop the server, or give the CLI its own dir. CI eval
runs are designed to use a standalone dir.

**WAL keeps growing / restart takes long (`spans_recovered` is huge)**
Compaction is lagging or off. Check `--compact-interval-secs` isn't `0`, check
stderr for `compaction pass failed (will retry)` (a failing flush retries on the
next tick and the WAL is *not* truncated — again usually disk), and consider a lower
`--seal-threshold` (a segment only becomes flushable once sealed, so a
50k-span threshold on a low-rate stream keeps spans in the WAL/hot tier for a
long time — reads are unaffected, restart replay just costs more).

**A span I POSTed doesn't show in `GET /v1/spans`**
Check the ingest response first: a `400` means the payload never decoded (evald
answers 400 with the decode error; note OTLP-JSON must use the protobuf JSON
field names, e.g. `traceId`, `startTimeUnixNano`). A `200` means it is stored —
`GET /v1/spans` defaults to the most recent 100, so pass `?trace_id=` or a
bigger `limit`.

**`401 Unauthorized` (`evald: unauthorized — set Authorization: Bearer <token>`)**
The bearer-token gate is armed (`--auth-token` / `EVALD_AUTH_TOKEN` /
`--auth-token-file`) and the request had no valid token. Add
`-H "Authorization: Bearer <token>"` (curl), set
`OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <token>"` (OTLP SDKs), or, for
gRPC, send the token as `authorization` metadata. A browser navigation can't send
the header — reach the SPA over a loopback tunnel or a header-injecting proxy. If
you did **not** mean to require auth, unset the flags/env — an unset or
present-but-empty `EVALD_AUTH_TOKEN=` / `EVALD_AUTH_TOKEN_FILE=` value leaves auth
**off** (it does not fail boot). If `serve` **exits at boot** with `auth
configuration error: …`, a token is under 16 chars or has a non-ASCII character, or a
`--auth-token-file` you named was unreadable or held no usable tokens — an
explicitly-named token file that is empty or comment-only is treated as a
misconfiguration and fails loud (rather than silently starting wide-open).

**`400 evald: SQL error: only read queries are allowed …`**
The `/v1/sql` guard rejected a non-read statement (`INSERT`, `COPY`,
`EXPLAIN ANALYZE`, multi-statement bodies). The endpoint is read-only by design;
writes go through OTLP/scores only.

**SQL results end at exactly 1 000 rows / `"truncated": true`**
The default result cap. Pass `{"limit": …}` (max 100 000) or aggregate in SQL
instead of pulling raw rows.

**`eval compare` says `no aggregate scores found for run …`**
Wrong run id, a different `--data-dir` than the one `eval run` wrote to, or the
run predates score persistence. `eval run` prints `run_id:` — compare exactly
those, with the same `--data-dir`.

**Judge eval fails with a missing-key error / makes no calls**
Tier-3 judges need a binary built with `--features judge` **and**
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` at run time (read at call time only).
Use `evald eval run --estimate` first — it needs neither. Re-runs hit
`judge_cache.redb` and are free.

## Security posture

> **Data at rest holds whatever you sent it.** By default evald stores prompts and
> completions verbatim. If that content carries PII, turn on
> [redaction](#redaction) — it rewrites detected values before the WAL, so they
> never reach any on-disk tier. Redaction is a content control, not a substitute
> for filesystem permissions or volume encryption; use both.

- **Listens:** one TCP listener, `127.0.0.1:4318` by default
  (`--otlp-http` / `EVALD_OTLP_HTTP_ADDR`; the OTLP/gRPC `:4317` sibling is the
  same). Loopback-only unless you deliberately bind wider.
- **Authn/z: optional bearer-token gate, OFF by default.** With no token
  configured, anyone who can reach the port can write spans, write scores, and read
  everything (including `POST /v1/sql` full scans) — the intended threat model is
  untrusted *input* on a *trusted* network (laptop, locked-down CI runner). Arm the
  gate with `--auth-token <tok>` (repeatable), `EVALD_AUTH_TOKEN` (comma-separated),
  and/or `--auth-token-file <path>` (one token per line; blank lines and `#`
  comments ignored) to require an `Authorization: Bearer <token>` on **every** HTTP
  request (OTLP ingest, `/v1/*`, and the SPA) and **every** OTLP/gRPC request (the
  `authorization` metadata). A missing/wrong token is `401` (HTTP) /
  `UNAUTHENTICATED` (gRPC). Tokens must be **≥16 printable-ASCII chars** (rejected
  at boot otherwise), are matched by SHA-256 digest, and are **never logged**. The
  three sources **union** (a `--auth-token` flag is one whole token, `EVALD_AUTH_TOKEN`
  is a comma-separated list, `--auth-token-file` is one per line), so you can rotate
  (add a new token, drain clients, remove the old) and revoke per client. A
  present-but-empty `EVALD_AUTH_TOKEN=` placeholder leaves auth **off** (it does not
  fail boot). It is a shared-secret gate, **not** per-user identity — every valid token
  has full access.
- **Startup tells you if you're exposed:** binding a listener off-loopback with no
  token logs a loud `WARN` (pointing here); with a token it logs `auth: … ARMED`
  (the token *count* only — never the tokens).
- **TLS:** evald does no TLS. A bearer token over plaintext HTTP is only as private
  as the network, so if the path from client to evald is untrusted, terminate TLS at
  a reverse proxy (nginx, Caddy, an mTLS mesh, or the sibling **edgeguard** front
  door) in front of `:4318`/`:4317` — even with the token gate on.
- **Exposing it — pick one:** (a) the built-in token gate, on a trusted-transport
  segment or behind a TLS-terminating load balancer; (b) a reverse proxy that
  terminates TLS *and* authenticates; (c) keep evald loopback and let the
  separately-licensed ee fleet layer handle multi-node ingest/auth. Caveat for the
  gate: a **browser** top-level navigation can't send an `Authorization` header, so
  reach the SPA over an SSH tunnel to loopback or via a proxy that injects the
  header. **Probes need no token** — `/healthz` and `/readyz` are exempt from the
  gate (a kubelet sends none), so point them straight at those endpoints rather
  than at an authenticated `/v1/*` path. `/metrics` *is* gated; give Prometheus the
  token via `authorization:` / `bearer_token_file:`. See § Monitoring.
- **Input hardening that is built in:** decompressed-size body cap (16 MiB, the
  gzip-bomb guard), parsed-statement read-only SQL gate, bounded ingest channel
  (429 shed), clamped read limits.
- **Secrets:** the server needs none. Judge API keys are read from the
  environment at call time by the `eval` CLI only — never logged, never
  persisted, never part of the cache key
  ([`src/judge.rs`](../src/judge.rs)).
- **Data at rest:** WAL/Parquet/redb are unencrypted local files — protect the
  data-dir with filesystem permissions/volume encryption. Span payloads can
  contain prompts/completions; treat the data-dir as sensitive.
- Vulnerability disclosure: [`SECURITY.md`](../SECURITY.md).
