# Changelog

All notable changes to evald are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project aims to
follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] - 2026-09-22

### Added
- **Cost is now derived from the model and token counts** instead of being empty for most
  spans. `gen_ai.*` has no cost attribute and most instrumentors do not set `llm.cost.*`, so
  `cost_usd` was `NULL` for nearly everything and `evald cost` showed tokens only. Now a
  price table (a trimmed copy of the LiteLLM `model_prices_and_context_window.json`, compiled
  into the binary, `<commit>@<date>` versioned) prices every span that has a model and token
  counts and **no cost of its own**; a cost the instrumentor reported is never overwritten.
  `--price-table <file>` (on `serve` and `cost`) lays your file over it (fine-tunes,
  corrections), is re-read when it changes, and makes no network call: refresh it yourself.

  - **Stored with the span, no format change, nothing extra per span.** `cost_usd` is the
    existing column and the only thing a priced span gains. A derived cost is told from a
    reported one at read time — a reported cost always comes with the attribute it was read
    from (`llm.cost.total` / `gen_ai.usage.cost`), which `raw_attributes` keeps — so no stamp
    is written: a per-span `evald.cost.source=derived@<version>` was tried and cost ~50 bytes
    and two allocations per priced span through the WAL, the log line and the hot tier, most of
    the measured ingest cost of pricing (`docs/BENCH-AB-GUIDE.md` §6). The table version lives
    once per block instead (`evald.price_tables` in the Parquet footer: the table in force when
    the block was flushed, the union for a merge) and on `GET /v1/meta` (`price_table`).
    `evald.cost.basis` is still stored, only when the counts were read the non-default way.
    `tests/format_freeze.rs` passes unmodified.
  - **Corrected at query time, never by rewriting.** `evald cost --price-table t.json`
    recomputes the report from the stored token counts (spans whose cost was reported keep
    it).
  - **A model with no price is `(no price)`, not `$0`.**
  - **Cached and reasoning tokens are priced exactly once.** Sources disagree on whether the
    input count *includes* cached tokens (the OpenTelemetry conventions and OpenAI: yes;
    Anthropic's own API and Bedrock Converse: no). evald decides from the span's numbers
    (the reported total, or a cache count larger than the whole prompt) rather than from a
    list of SDKs; `docs/INSTRUMENTATION.md` has the per-source table with the evidence and a
    confidence for each row, including the ones that could not be verified. Tiered
    (long-context) prices, cache read/write rates and a separate reasoning rate are applied.
  - **The hot path stays cheap:** one table handle per request, one hash lookup per span, and
    plain arithmetic; name normalisation only runs when the exact model id is not in the
    table. A derived cost is rounded to 1e-12 USD: a count is an integer and a rate carries a
    few significant digits, so the product is a short decimal, and the digits binary floating
    point adds beyond it would otherwise be stored, logged and summed for every priced span.
  - Also reads the current OpenTelemetry spellings `gen_ai.usage.cache_write.input_tokens`
    and `gen_ai.usage.reasoning.output_tokens` (the older names still work).
  - A table older than 90 days logs a warning.

- **LLM usage on `/metrics`, and `evald latency`.** Until now `/metrics` described evald itself; it
  now also carries what the LLM calls it has accepted cost: `evald_llm_requests_total`,
  `evald_llm_request_errors_total`, `evald_llm_cost_usd_total` (with
  `evald_llm_spans_without_cost_total`, so a partial cost is visible rather than silent),
  input / output / cache-read / cache-write / reasoning token counters, a
  `gen_ai_client_operation_duration_seconds` histogram, a
  `gen_ai_client_operation_time_to_first_chunk_seconds` histogram for spans that carry a
  time to first token, and `evald_eval_score_mean{evaluator}` (a rolling mean of the last 1024
  scores). Labels are `gen_ai_provider_name`, `gen_ai_request_model` and `service_name` only, never
  user or session; each is capped and folds into `other` past the cap (`--metrics-model-cap`,
  default 100; `evald_usage_labels_folded_total` says when), with a hard ceiling of 2048 label
  tuples, so the series count is bounded whatever the traffic. The histogram bounds are the
  OpenTelemetry GenAI advisory boundaries, extended to 327.68 s for reasoning models.

  They are recorded at the commit fsync, one lock per batch and no per-span allocation, for
  accepted spans only: recovering the WAL at startup does not re-count. Counters are since
  process start and **approximate under exporter retries** (a re-sent batch is counted twice);
  `POST /v1/sql` over the store is exact. `--no-usage-metrics` turns them off; example
  Prometheus alert rules are in `docs/OPERATIONS.md`.

  `evald latency [--by model|provider|service]` prints **exact nearest-rank** p50 / p95 / p99
  over LLM spans (the value at rank `ceil(p·n)`: always a span that happened, never an
  interpolation), and, where spans carry one, the time to first token — read only from span
  attributes (`gen_ai.response.time_to_first_chunk`, `ai.response.msToFirstChunk`,
  `ai.stream.msToFirstChunk`, `time_to_first_token_ms`), shown as `unknown` otherwise and never
  estimated. The equivalent SQL is documented and tested. Nothing new is stored: the on-disk
  format is unchanged.
- **`gen_ai.evaluation.result` in and out.** The OpenTelemetry GenAI evaluation-result event, as
  a span event in an OTLP traces export, is now stored as a score targeting the span it is
  attached to (`gen_ai.evaluation.name` → `name`, `.score.value` → `num_value`, `.score.label`
  → `str_value`, `.explanation` → `comment`), on the HTTP protobuf, OTLP-JSON and OTLP/gRPC
  receivers alike. The score id is a hash of trace, span, evaluation name and event time, so an
  exporter retry overwrites instead of duplicating. A malformed event (no evaluation name, or no
  value, label or `error.type`) is dropped and counted in `evald_eval_events_malformed_total`
  without failing the spans in the same request; strings are length-capped. A span with no
  events costs one emptiness check. `evald scores export --format gen_ai-event` writes stored
  scores back out as one event object per line. No stored column changed, and the format-freeze
  test passes untouched. The event is at *Development* stability upstream; this build follows
  semantic-conventions-genai at commit `cc07f72` (2026-09-21, semconv v1.44.0). **Not
  ingested:** the same event sent as an OTLP *log record* (evald has no logs endpoint), and
  `gen_ai.response.id`, which is not stored.
- **`--junit <path>` on `eval run`, `eval compare` and `suite run`.** A JUnit XML report that
  CI test tabs render natively: one test case per evaluator (`eval run`), per evaluator delta
  (`eval compare`, failing exactly when the gate does, with the delta and, under
  `--significance`, the p-value and confidence interval in the message) or per suite case plus a
  final `suite pass rate` case (`suite run`). It is written when the gate fails, and when the
  command errors (as a single `<error>` case), and it never changes the exit code; if it cannot
  be written after a successful run the command fails, so a job that asked for a report does not
  pass without one. Text from models and judges is escaped and any character XML 1.0 forbids is
  replaced, so the file always parses. Per-case `time` is `0.000` (evaluators are not timed one
  by one); the suite carries the command's wall-clock time. A machine-readable JSON output for
  these commands is not added here.
- **The on-disk format is frozen, versioned and documented** — `docs/FORMAT.md`, and
  `evald migrate`. Every data-dir now carries a `FORMAT` marker naming the format version,
  the evald that created it, and when. The policy it states:

  - A release reads its own format **and the one before it**, so an upgrade is a binary
    swap and never an export-and-re-import.
  - A data-dir written by a **newer** evald is refused, naming both versions, rather than
    opened optimistically — an older binary that ignores what it does not recognise is how
    a store ends up readable only by the thing that corrupted it.
  - A data-dir written **before** the freeze is stamped in place. No block is rewritten and
    nothing moves; the layout was already format 1, only the statement of it was missing.
    `evald migrate --dry-run` reports it first.

  The promise is enforced rather than asserted: `tests/format_freeze.rs` reads a
  **committed** format-1 data-dir — WAL segment, flush block, merged block, both redb
  files — on every CI run, so a change that breaks compatibility fails the build instead of
  a user's upgrade. Blocks also carry their own provenance in Parquet key-value metadata
  (`evald.format`, `evald.block_kind`, `evald.seqno_lo/hi`, `evald.merged_from`,
  `evald.writer`), so anything holding just a file knows what it is.

- **Cold-to-cold compaction** (`--cold-merge-*`, `evald compact`) — cold blocks now merge
  into fewer, larger ones instead of accumulating forever.

  Every sealed WAL segment became a Parquet block and nothing ever merged them, so the
  block count only grew — and with it the set of files a query has to open, until scans
  died with `Too many open files`. The soak gate hit exactly that at ~18,600 blocks
  (`docs/SOAK.md`). Now an hour partition's blocks merge once enough **of a similar size**
  have accumulated (default 16), and a fully-past UTC day collapses once nothing has been
  written into it for an hour. Merging only within a size class is what keeps write
  amplification logarithmic rather than quadratic; a merged block is capped at 1,000,000
  spans so retention, which drops blocks whole, stays granular.

  A merge commits exactly like a flush — write, fsync, rename, then **one** redb
  transaction swapping inputs for output — and its inputs are unlinked only after a grace
  window, so a query that listed them moments earlier is never pulled out from under. A
  crash at any point leaves either the old blocks or the new one indexed, never both.

  The grace is measured from retirement, not from when a block happened to be written: the
  sweep reads mtime, and each input is stamped with the current time immediately *before*
  the commit that unreferences it. That ordering is what makes the window airtight — a
  failed stamp returns before the commit, so nothing is retired and nothing can be swept
  early, and a crash there leaves the inputs still indexed, where the sweep never looks.
  Stamping afterwards would leave an instant in which an input is unreferenced but still
  carries its original mtime, and the next sweep would delete it with no grace at all.

  Measured: a 600-block store collapses to 2 blocks, 4.4 MiB to 0.1 MiB.

- **A query holds at most 64 cold blocks open**, whatever the block count. The scan
  registers one file per block and opened them concurrently, so descriptors tracked the
  store's size. Measured: a scan of 2,000 blocks peaked over 1,000 descriptors and failed
  under `ulimit -n 256`; it now peaks at exactly 64 and runs under `ulimit -n 96`.
  `tests/fd_ceiling.rs` pins both this and the block-count bound, reproducing the original
  `EMFILE` when either is removed.

- **New observability for the cold tier**: `evald_cold_blocks` (gauge),
  `evald_cold_merges_total`, `evald_cold_blocks_merged_total` and
  `evald_cold_merge_failures_total`, plus the same fields on `GET /v1/stats`. A block count
  climbing while merges stay flat is the shape of the failure above, now visible before it
  bites.

- **Score rollup — what a *trace* scores when its spans are scored**
  (`GET /v1/traces/{trace_id}/scores`, `--rollup name=fn`). Closes the model gap `PLAN.md`
  carried as an open Beta blocker: a Score attaches to a span, but an agentic task emits a
  multi-span trace with no single canonical span to hang the answer on, so "the trace's
  faithfulness" was **undefined** — and `/v1/scores?trace_id=` returned nothing at all for a
  trace whose every span was scored.

  The semantics, stated in full in `PLAN.md` §2.4:

  - **Writing is unchanged; the question is asked at read time.** Nothing is promoted on
    write, so no stored score changes meaning and there is nothing to migrate. A score
    attached later is picked up with no rebuild.
  - **Measured beats derived.** A score on the trace itself is authoritative and is never
    overridden by a computed one.
  - **A derived value says so**, carrying `measured: false`, the function, the contributor
    count and the contributing span ids. A rolled-up number is an inference, not a
    measurement; rendering it identically to a measured one would undercut the same posture
    that motivates the Welch's-t gate and judge calibration.
  - **Absent stays absent.** A name nothing carries yields no score — never `0`, never a
    fabricated pass.
  - **A scored span is authoritative for its subtree.** For
    `root → {retrieve, synthesize → {llm_1, llm_2}}`, averaging a score on `synthesize`
    alongside its children's double-counts the same work, so the walk takes the shallowest
    carrier of each name per branch — per name, so one trace can roll `faithfulness` from
    one depth and `toxicity` from another.
  - **The function is per score name and declared**, never inferred: `mean` (default,
    numeric), `all` (default, boolean), `min`, `max`, `sum`, `any`. One global function
    cannot be right for every metric — cost wants `sum`, a pass/fail wants `all`, and a CI
    gate usually wants `min`. Categorical and free-text scores are skipped rather than
    averaged. The choice is made from the carriers' **common** type, not the last one the
    walk happened to see, and `data_type` travels with the result — so `all` over a
    pass/fail stays a boolean when it is written back rather than flattening into a
    number that no longer says whether `0` meant "a step failed" or "the mean was zero".
  - **Spans in a parent cycle still count.** Malformed instrumentation (or a replayed span
    id) can leave every span in a component pointing at another as its parent, so the
    component has no root and a root-first walk never reaches it. Those components get a
    deterministic entry point instead of silently contributing nothing — an absent score
    and a dropped one read identically and mean opposite things.

  The `RunItem` attach point is settled with it: item↔**trace**, never item↔span, so the
  1:1 assumption in the offline-eval spine holds. Online eval is no longer blocked on this.
- **PII redaction on the ingest path** (`serve --redact email,credit_card,…` or `--redact all`)
  — strips sensitive values from prompts, completions and span attributes (recursing through
  nested JSON) **before anything is written**. Classes: `email`, `credit_card`, `ssn`,
  `phone`, `ip`, `api_key`, `jwt`, plus `--redact-custom name=regex`. `--redact-action`
  selects `redact` (`[REDACTED:email]`), `hash` (`[email:9f2a…]` — unrecoverable but stable,
  so equal values stay groupable) or `drop`. Off by default.

  The disk floor is enforced ahead of both: the blob offload writes, so checking only at the
  append would let a refused ingest leave orphaned blob files on an already-full volume.

  The guarantee is placement: redaction runs **before the WAL append and before the blob
  offload**, and the WAL is the ACK boundary — so a detected value never touches disk in any
  tier, not the WAL, not a blob, not a Parquet block. A test walks every byte of the data-dir
  after an ingest and asserts the raw values are absent, because asserting only that the read
  path is masked would also pass for a display filter. The corollary is that it is
  **irreversible**, which is why it is opt-in.

  A `--redact-custom` pattern may contain capture groups of its own
  (`ticket=(ABC|DEF)-[0-9]+`); rules are resolved by their recorded group number, so one
  rule's internal groups cannot shift the rules declared after it — which would otherwise
  make a later rule stop matching entirely, or attribute its hits to another rule's label
  and Luhn gate.

  Detectors prefer a missed match to a false one, because a redactor that mangles ordinary
  text gets turned off: **credit cards are Luhn-checked** (so `card 4242424242424242` is
  redacted and the `ref 1234567812345678` beside it is not), API keys match published vendor
  shapes rather than "long random string", and `ip` is documented as the one irreducibly
  false-positive-prone class. Custom regexes compile at startup, so a bad pattern fails the
  process instead of silently never matching under load.

  Not scanned, deliberately: `user_id`, `session_id`, `service_name` and the span name —
  identifiers the caller chose, usually already opaque, and rewriting them would silently
  break `evald cost --by user|session`. Attribute keys are left alone too, since queries are
  written against them.

  New series `evald_redactions_total{rule}` counts what was rewritten per rule. It counts
  occurrences in the stored representation: `normalize` promotes `input.value` into
  `input_value` while also preserving it in `raw_attributes`, so one email in a prompt is
  rewritten — and counted — in both copies.

  Both ingest front doors (HTTP and gRPC) now call one `Store::prepare_for_storage` step
  rather than the redact and offload stages separately, so neither can grow a path that
  skips one.
- **Automatic retention + disk guardrails** — `evald serve --retention 30d` runs the block
  sweep on a timer (`--retention-interval-secs`, default hourly). **Unset, nothing is ever
  deleted**: evald does not remove a user's data by default. A block is dropped only when
  every span in it predates the window, index entry first then `unlink`, so reclamation
  stays `O(unlink)` and a crash mid-sweep leaves an orphan the next open collects.

  The guardrail is the safety valve for when retention is not enough — or when something
  else fills the volume. Below `--disk-warn-free` (default 1GiB) it logs; below
  `--disk-min-free` (default 256MiB) it refuses ingest with `503 + Retry-After: 30`,
  cleanly, *before* a write hits `ENOSPC` part-way through a WAL segment or a Parquet
  flush. That is a known failure class in embedded trace stores: an ingest pipeline that
  runs until `ENOSPC` hits mid-write, jamming the store rather than shedding cleanly.

  Deliberate choices worth knowing: it is `503`, not the `429` a backlog shed returns —
  both are retryable to an OTLP exporter, but `429` means "you are sending too fast" and a
  full disk is neither the client's fault nor fixable by backing off, so the two stay
  distinguishable in exporter metrics. Free space is space available to an **unprivileged**
  process (`statvfs` `f_bavail`), so the root reserve is not counted as usable. Sampling is
  on a timer, not per request, so an append pays one relaxed atomic read. And the guardrail
  **fails open**: if free space cannot be read it never blocks ingest and says so once —
  refusing writes because the disk could not be *measured* would invent an outage.

  New series: `evald_disk_free_bytes` (omitted entirely when unsampled — a `0` would read
  as "disk full" to every alert), `evald_disk_blocked`, `evald_spans_disk_blocked_total`,
  and `evald_retention_{sweeps,blocks_dropped,bytes_reclaimed}_total`. `/v1/stats` carries
  the same fields.

  This adds `libc` as a direct dependency for `statvfs` — **no new crate in the build**
  (DataFusion already pulls it), and the crate's only `unsafe`, in one documented function.
  `rustix` would have been safe but reaches the tree solely through `tempfile`, a
  *dev*-dependency, so taking it would have added a crate to the shipped binary.
- **Prometheus metrics + Kubernetes probes** — `GET /metrics` (text exposition format
  0.0.4), `GET /healthz` (liveness) and `GET /readyz` (readiness). Ten series cover the
  ingest pipeline: spans durably ACK'd, spans shed, hot-tier backlog and its bound, whether
  ingest is shedding, channel depth, compaction passes completed/failed, and WAL bytes.
  `evald_spans_ingested_total` is counted at the fsync that commits a group, so it tracks
  the durability boundary rather than requests received. Hand-rolled against the exposition
  format — no metrics crate, so the default build stays pure-Rust and air-gapped.

  The probes are **exempt from the bearer-token gate** (a kubelet sends no `Authorization`
  header, and a liveness probe that 401s is a crash loop); `/metrics` stays behind it, since
  a scrape exposes ingest rates and backlog depth. `/healthz` deliberately does not consult
  the store — failing liveness on a slow disk would have the kubelet kill a process that
  still holds a good WAL. `/readyz` consults exactly one condition, whether the writer task
  is alive; **shedding is not a readiness failure**, because `429 + Retry-After` is the
  documented backpressure contract and a loaded node should keep applying it rather than
  leave rotation. Whole-store counts (total spans, total scores) are deliberately absent:
  both need a full scan, and paying for one every 15 seconds would make the monitoring
  endpoint the outage.

  `GET /v1/stats` gains the same three new counters (`spans_ingested`, `compactions`,
  `compaction_failures`) so the JSON and the scrape stay one source of truth.
- **Legacy indexed `gen_ai` message normalization** — spans instrumented with the
  deprecated *indexed* shape (`gen_ai.prompt.{i}.role` / `gen_ai.prompt.{i}.content`
  and `gen_ai.completion.{i}.*`), still emitted by widely deployed instrumentation such
  as Traceloop OpenLLMetry, now have their input/output reconstructed into the same
  messages array the modern `gen_ai.input.messages` / bare `gen_ai.prompt` forms carry.
  Previously these spans' prompts and completions were dropped from `input_value` /
  `output_value` (surviving only as scattered raw attributes) — so they were invisible
  to the read APIs, the console, cost/eval curation, and SQL. The non-indexed keys still
  take precedence, so existing spans are unchanged.

- **Soak-test gate — the GA storage blocker (`tests/soak.rs`).** `PLAN.md` §6 named one gate
  before GA: sustained ingest + kill-9 crash recovery + compaction under load, with
  fsync-correctness verification. This is it, and CI runs it on every push.

  `tests/crash_recovery.rs` already killed the server once and proved no ACK'd span was lost.
  That catches a store that is wrong immediately; it does not catch the failures that only
  appear after several crashes — a recovery that loses a little each time, a compaction that
  stops committing once it has been interrupted (leaving an unbounded WAL behind a store that
  still answers every query correctly), or a hot/cold dedup that double-counts only after both
  tiers have been rebuilt from a torn state. Each is invisible at one cycle and obvious at ten.

  Each cycle drives concurrent ingest, waits for compaction to commit new Parquet blocks
  **while that load is running**, SIGKILLs mid-flight, and reopens the store in a fresh
  process. Four properties are asserted after every cycle:

  1. **No ACK'd span is ever lost** — every id that was handed a `200` is still in the store
     at the end, checked by **identity**. The obvious form, recovered rows ≥ ACKs, is not
     sufficient: recovery legitimately returns a few surplus rows (spans fsynced just before a
     kill whose `200` never got home), so a lost ACK'd row can hide behind one of them and the
     count still clears — the gate reporting success with an fsync-durable span missing. The
     count form still runs per cycle as a fast check on gross loss; the identity check runs
     once at the end, where it is equally strong because nothing ever re-adds a span. This is
     the fsync-correctness verification.
  2. **No span is double-counted** — `COUNT(*) == COUNT(DISTINCT span_id)`.
  3. **Recovery never goes backwards** — rows after cycle *k* ≥ rows after cycle *k-1*.
  4. **Compaction keeps working under load** — every cycle must commit a Parquet block that
     did not exist when that cycle began. Deliberately per-cycle: a store that quietly stops
     compacting after its first unclean shutdown passes 1–3 indefinitely.

  That surplus is measured, not hypothetical — every run shows a handful of rows — which is
  precisely why property 1 had to become an identity check rather than a comparison of totals.

  Verified to fail, not just to pass. All four properties were induced and caught; three
  through real mechanisms (a destroyed WAL, genuine duplicate span ids over the wire,
  compaction switched off). Property 3 is a wiring proof and is labelled as one. Duration,
  cycles and worker count come from `EVALD_SOAK_SECS` / `EVALD_SOAK_CYCLES` /
  `EVALD_SOAK_WORKERS`; the assertions do not change with the clock. Ignored by default so
  `cargo test` stays fast. Method, knobs, measured result and the failure proofs are in
  `docs/SOAK.md`, which is published to the mirror — a self-hoster can run the same
  verification rather than take the durability claim on trust.

  **The gate found a defect on its first full-length run, which is the point of it.** At CI
  duration it passes. At GA duration `evald query` dies with `EMFILE` — `Too many open files`
  — while planning a count, nine cycles and ~18,600 committed blocks in. No durability
  property failed: every completed cycle reported no loss, no double-count and no regression.
  The cold tier never merges blocks, and the SQL path registers one listing URL per block, so
  the open-file working set grows with every block ever committed; the ceiling is the process
  file-descriptor limit, measured at 716 blocks under `ulimit -n 512` against ~18,600 under
  20,000. An apparent throughput decline across those cycles turned out to be
  confounded by the harness's own block-directory polling and is not quoted as a result. The data stays intact — this is read availability, not durability — but
  nothing bounds the block count. Recorded in `docs/SOAK.md` rather than smoothed over; the
  fix is not in this change.

  `EVALD_SOAK_SEAL` exists because of it: long runs raise the seal threshold so the four
  durability properties stay measurable instead of every long run stopping on that ceiling.

  The rig `tests/crash_recovery.rs` had grown (server spawn with the whole `EVALD_*` namespace
  stripped, hand-rolled localhost POST, kill-9, read-back through `evald query` with a
  deadlock timeout) moved to `tests/common/mod.rs` and is now shared by both tests rather than
  duplicated.

- **The hot-tier engine question is closed** — `HOT_TIER_DECISION.md`. `PLAN.md` §6 called
  a young LSM on the crash-durable write path the single biggest storage risk and gated the
  choice on data; the answer is **no LSM**, and the data is now in the repository.

  The hot tier is a read cache over spans the write-ahead log has already made durable. Its
  size is one compaction interval of ingest, capped by `--max-hot-spans`, which sheds rather
  than grows. Measured at that cap (`tests/hot_tier_bounds.rs`, release, 1 KiB spans):
  1.9 KiB resident per span, 4.5 s to replay a million spans on restart, 40 ms for a trace
  lookup across them, all linear in span count. The three things an LSM would buy — spill to
  disk, bounded recovery, indexed reads over the un-compacted set — are each negligible at a
  realistic window, and would cost a second on-disk format three weeks after the first was
  frozen, plus compaction stalls back on the ACK path.

  `docs/CONFIG.md` now documents `--max-hot-spans` (it was undocumented) with the sizing
  arithmetic the measurement produced. That arithmetic is what moved the default: the
  previous bound of one million spans is roughly **1.8 GiB** in spans alone at 1 KiB
  payloads, past the limit the project's own manifests set, so the default is now 300,000
  (see *Changed*, below). Size a container limit against the whole-process table there
  rather than against the tier alone — below it the kernel arrives first, with an OOM kill
  instead of the backpressure the bound exists to provide.

### Changed
- **The per-span ingest log line moved from `info` to `debug`** — a default `evald serve`
  now logs **one line per request**, not one per span. This is a visible behaviour change:
  anything that read those lines needs `RUST_LOG=evald=debug` (or
  `RUST_LOG=evald::ingest=debug` for just the ingest detail). The fields are unchanged —
  dialect, trace/span id, name, `oi_kind`, model, provider, token counts, `cost_usd`,
  duration, attribute count.

  It was not a logging preference, it was a third of the ingest hot path. A callgrind
  profile of `evald serve` ingesting 3,000 spans over OTLP/HTTP (compaction off) measured
  **~113,000 instructions per span at the default filter and ~75,000 with `RUST_LOG=warn`**:
  formatting and ANSI-colouring those ~14 fields cost **~38,000 instructions per span**,
  with `Event::dispatch` inclusive at ~28% of all instructions. Every default server paid
  that on every span, at whatever rate spans arrived, whether or not anyone was reading
  stderr — and since `bench/run.sh` sets no `RUST_LOG`, the published throughput numbers
  paid it too.

  The per-request `info` summary (`resource_spans`, `spans`) stays exactly where it was, so
  the default server still shows that ingest is happening. The level is now tested once per
  request rather than once per span — the `debug!` macro's own first gate, hoisted out of
  the loop — so a filtered-out per-span line costs nothing per span.

  Measured end to end on a 4-vCPU VM (8 connections, two rounds, one release binary): the
  new default and `RUST_LOG=warn` are within 0.5% of each other at ~33.5 µs of server CPU
  per span, while restoring the per-span line costs **38.3 µs — +14% CPU and −7%
  throughput**. That box is bound by fsync rather than CPU, so it is a floor.

  `docs/OPERATIONS.md` § Logs and `docs/CONFIG.md` § Logging document the default and the
  opt-in; `docs/BENCHMARKS.md` and `bench/README.md` now state which filter a benchmark
  number assumes. The evald rows in `docs/BENCHMARKS.md` predate this change and were
  measured with the per-span line on, so they understate a default server; they are marked
  as such rather than restated from different hardware.

- **`--max-hot-spans` now defaults to 300,000, down from 1,000,000.** This is a behaviour
  change on upgrade: a store that sustains more than roughly 60,000 spans/s at the default
  5 s compaction interval will start shedding (`429 + Retry-After`) where it previously did
  not. Set `--max-hot-spans` explicitly to keep the old bound.

  The old default could not do its job. The bound exists so that a lagging compactor sheds
  ingest instead of exhausting memory, but at the measured ~1.9 KiB resident per 1 KiB span
  (`HOT_TIER_DECISION.md` §3) one million un-compacted spans is about **1.8 GiB** in spans
  alone — past the 1 GiB limit the project's own k3s and Helm manifests set for a node,
  whose comment claimed it "covers the hot tier riding an ingest burst". The OOM killer
  arrived before the bound could shed, which is exactly the failure the bound is there to
  prevent.

  The new default is sized by measuring the **whole process** at a full cycle — tier filled,
  a bounded read, compaction, a cold merge, a SQL aggregate — not the tier alone, since
  those transients land together on a busy node. At 300,000 the peak is ~852 MiB including a
  serve process's own ~114 MiB, or 83% of that 1 GiB limit, leaving ~172 MiB for a heavier
  query. 350,000 reaches 92% and 500,000 exceeds the limit outright;
  `HOT_TIER_DECISION.md` §4 and `docs/CONFIG.md` carry the table and the arithmetic for
  sizing the bound and the memory limit from each other.

  Headroom over normal operation: six sealed segments at the default seal threshold, and
  about six times the steady-state window of the measured fleet deployment.

### Fixed
- **A bounded SQL query no longer materialises the whole result.** `POST /v1/sql` and
  `evald query` both pass a row cap, but the cap was applied *after* `collect()` had already
  built every row the plan produced — so `SELECT * FROM spans` with a limit of 1 assembled
  the entire store in Arrow memory to return one row, then dropped it. The cap is now a
  `Limit` in the plan, so the executor stops pulling batches instead. It fetches one row past
  the cap, which is what keeps `truncated` answerable without paying for the rest. `EXPLAIN`
  is exempt: it has to be the root of its own plan, and its output is a handful of lines.

  This is the same collect-then-truncate as the hot-tier read below, one layer up; the
  bounded-read claim was only half true while the SQL path still did it.

- **A bounded read no longer copies the whole hot tier.** `Store::query` collected every
  matching span and truncated afterwards, so `GET /v1/spans?limit=100` against a full hot
  tier allocated a second copy of it — 1.6 GiB, to return 100 rows — and then dropped it.
  It now keeps a bounded selection of the newest rows as it scans, cloning only survivors,
  and a hot span that loses cannot let its cold twin through (identity still beats rank).
  Measured at a million un-compacted spans: **1.26 s against 3.27 s**, with the extra copy
  gone. Unbounded reads (`GET /v1/traces/{id}`, counting the store) keep collecting and
  sorting, which is faster when there is nothing to bound.

- **The read path's dedup set is a quarter of the size.** A read keys every resident span so
  a cold block cannot re-emit one the hot tier already has. That key was a pair of cloned
  `String`s — 48 bytes in the table and two heap allocations per span, about 0.3 KiB all in.
  The ids evald stores are a 16-byte trace id and an 8-byte span id written as lowercase hex,
  so the key is now those 24 bytes decoded back, held inline: **~0.07 KiB per resident span**,
  measured, with no heap in the canonical case.

  It is exact, not hashed. A fixed-width hash would be smaller still and would put a silent
  wrong answer — a span missing from a result — on the read path, which is the one failure
  this store refuses to have and the one no test could reliably catch. An id that is not
  canonical hex keeps its exact text instead, as a pair rather than a joined string, so no
  separator can make two different identities compare equal.

  Two smaller wins came with it: the set is sized up front for an unfiltered read, so it
  never holds an old and a new table at once while doubling; and the cold phase no longer
  clones the hot set to stay retry-safe, which was a second copy of the largest structure on
  the path, allocated on every read.

- **The open-time sweep no longer logs one line per file.** It reports a single summary
  instead. On a store that merges, every merge input is unreferenced between its commit and
  the grace-window reclaim, so a process that exits inside that window left thousands of
  files for the next open to collect — which produced thousands of startup WARN lines, and
  could block `evald query` outright when its stderr went to a pipe the caller did not
  drain until exit.

## [0.2.0] - 2026-07-13

A redesigned console and a cleaner OSS/EE boundary.

### Added
- **New console** — the embedded UI is rebuilt as a Vite + React + TypeScript app,
  data-driven from a view registry: Overview, Traces (trace list → span tree → span
  detail with `gen_ai.*` attributes + token usage), Evals, Scores, a SQL console, and
  Cost (token/spend attribution grouped by model / provider / service / user). Still
  compiled to a static bundle and embedded in the binary — no Node at Rust build time,
  works air-gapped.
- **Standalone frontend image** (`mancube/evald-console`) — nginx serving the console
  with an SPA fallback and a `/v1` reverse proxy (`EVALD_API_URL`), for serving the UI
  apart from the store. See `docs/DOCKERHUB-CONSOLE.md`.
- **`docs/INSTRUMENTATION.md`** — how to point an LLM app at evald over OTLP
  (auto-instrumentation quickstarts, the attributes evald reads, attaching scores).
- **`GET /v1/meta`** — an edition/version handshake the console reads at boot.

### Changed
- **OSS/EE console split** — the OSS crate's embedded bundle now contains only the
  local-node surfaces; the Fleet surfaces live in the private `ee/` tree and are served
  by the fleet-query node from its own bundle. A build-time test guards the boundary so
  no Fleet view can re-enter the OSS bundle.

## [0.1.0] - 2026-07-13

First public release — an embedded OTel-native trace + eval store as a single static
binary: no container, no database, no Python runtime.

### Added — evaluators, judge & reporting
- **Full Tier-1 evaluator set** — closes the MVP "full set" gate. Adds four deterministic,
  zero-cost scorers to the eval config's `evaluators:`: `equals_numeric` (numbers equal
  regardless of textual form — `"1.0" == "1"`), `json_schema` (validate `output` against an
  inline JSON Schema; non-JSON fails, a schema gate must not wave garbage through), and the
  span-derived `latency` (`max_ms`) and `cost` (`max_usd`) gates, which read
  `latency_ms`/`duration_ns` and `cost_usd` from a dataset item's `metadata` (the span
  attributes materialized into the JSONL row). A missing field **skips** the item (never a
  fabricated pass/fail). `json_schema` uses the `jsonschema` crate with
  `default-features = false`, dropping its HTTP/file `$ref` resolvers — inline schemas only, so
  the default build stays fully offline (no `reqwest`/`hyper`). All four flow through the same
  aggregation, threshold gate, and `eval compare` significance path as the existing scorers.
- **Judge calibration + bias correction** (`evald eval calibrate --judge <name>`) — measure how
  far an LLM-as-judge drifts from human ground truth, **offline** against your own labels. Pairs
  each judge score with the human annotation on the **same span/trace** (evald is OTel-native, so
  the span id IS the join key — no extra bookkeeping) and reports the signed **bias**
  (`judge − human`), **MAE**/**RMSE**, **Pearson** correlation, a `(1 − α)` **confidence interval
  on the bias** (paired-difference Student's-t, reusing the dependency-free `stats` module), and —
  for bias correction — the affine map `human ≈ intercept + slope · judge` that realigns the
  judge's scale to the human's. A `RECALIBRATE` verdict fires when per-item disagreement (MAE) or a
  statistically-significant bias exceeds `--threshold`; `--fail-on-divergence` turns that into a CI
  gate against judge drift. Fully offline (no network, no key, no feature) and deterministic.
- **Tier-3 LLM-as-judge** (`judges:` in the eval config) — model-graded evaluation for criteria a
  deterministic check can't cover. Declarative,
  BYO-key, with built-in rails: `g_eval` (criteria grading), `qa_correctness` (reference-based),
  `answer_relevancy` (reference-free), `faithfulness` / `hallucination` and RAGAS
  `context_precision` / `context_recall` (RAG, over a per-item `context`), and safety rails
  `toxicity` / `bias` (1.0 = safe/unbiased). `evald eval run --estimate` previews the judge token
  usage + cost for a config **without** calling any provider (offline, no feature needed; skips
  already-cached items; indicative price table). Judge scores flow through the **same**
  aggregation, threshold gate, and `eval
  compare` significance path as the Tier-1 scorers, and are cached locally
  (`<data-dir>/judge_cache.redb`) so re-running an unchanged eval costs zero tokens.
  **Off by default**: the HTTP backend (Anthropic / OpenAI) is behind the `judge` cargo feature,
  so the default build pulls no `reqwest` and makes no outbound call; the key is read from
  `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` at call time only — never logged, persisted, or hashed
  into a cache key — and a `*_BASE_URL` env var points at an internal gateway. Model replies are
  parsed defensively (located JSON, score clamped to `[0,1]`). Example: `examples/eval/judge.yaml`.
- **`evald cost`** — token & cost attribution report. Groups the already-normalized
  per-span `cost_usd` + token counts by `--by model|user|session|service|provider`
  straight over the `spans` table (no new storage), surfacing `(untagged)` spans so
  partial tagging is visible. `cost_usd` shows when a span carried an `llm.cost.*`
  attribute; token totals are always available.

### Changed
- **`POST /v1/span_annotations` now returns Phoenix's `422 {"detail":[…]}`** on a
  malformed body (was `400` + text), matching the Phoenix/FastAPI validation-error
  contract so a Phoenix client's error handling works unchanged. Verified against the
  Phoenix OpenAPI spec: the `{"data":[…]}` envelope, the `LLM`/`CODE`/`HUMAN`
  `annotator_kind`, `result{label,score,explanation}`, `identifier` upsert, and the
  `{"data":[{"id":…}]}` success shape all match; the Phoenix REST contract defines **no**
  auth scheme, consistent with evald's unauthenticated OSS core.

### Fixed
- **Code-review hardening (PR #432).** A batch of correctness/robustness fixes from review:
  - **Significance test no longer manufactures certainty.** `welch_t_test` returned `t = ±∞`,
    `p = 0` for two constant-but-different samples (e.g. `2/2` vs `0/2`); it now reports that case
    untestable (`None`) so the gate falls back to the raw-delta check instead of claiming proof.
  - **`evald cost` totals are honest.** The `TOTAL` row is now a true unbounded aggregate over
    every span (was the sum of only the `--limit`-truncated rows), the breakdown notes when groups
    are hidden by `--limit`, and a bucket with mixed known/unknown `cost_usd` is flagged partial
    (`*`) instead of silently understating spend. Grouping is on the raw column (label applied in
    the projection), so a value literally named `(untagged)` no longer merges into the NULL bucket.
  - **Judges fail closed.** A malformed judge reply now scores the item `0.0` (failed) instead of
    skipping it, so an all-unparseable judge can't pass the threshold gate as "scored nothing";
    `JudgeSpec::validate` rejects an out-of-range `pass_threshold` or a `g_eval` judge with empty
    `criteria` up front (at every entry point, including the public `score_items`); and the cache
    canon is length-prefixed (delimiter-injection-safe).
  - **EE control plane.** Billing aggregates meters by unit before applying the free allowance
    (split meters can't dodge the bill); role-based authorization matches every error variant (fail
    closed); `VerifiedPrincipal` fields are private with a `pub(crate)` constructor so the tenant
    boundary is enforced, not conventional.
  - **OSS sync + release.** The mirror push protects `Formula/` (release-owned) from `rsync
    --delete`, a manual publish is gated to `main` (the gate reads `github.ref_*` via `env`, not
    inline `${{…}}`, to avoid shell template-injection on a crafted ref name), the release
    build/publish use `--locked`, and `.dockerignore` excludes `.env`/`secrets`/`local`.
  - **Crash test** now SIGKILLs during *active* ingest (a background sender stays in flight) and
    runs the recovery query under a timeout with captured stderr.
- **SQL/`evald query` now sees compacted (cold) spans.** The `cold_spans` table was a
  `ListingTable` over a directory/glob of the time-partitioned tree
  (`blocks/YYYY/MM/DD/HH/*.parquet`); DataFusion's directory listing didn't match the
  nested files, so every span that had been flushed to a Parquet block silently
  disappeared from `POST /v1/sql` and `evald query` (the in-memory hot tier still showed,
  masking it at small scale). The cold table is now registered from the **explicit
  committed block paths the redb index records** — never a raw dir scan — which also
  excludes orphan blocks from a crashed flush, matching the REST read path. Surfaced by
  the new kill-9 integration test (below). The REST `/v1/spans` path was unaffected.

### Added — packaging, CI & release hygiene
- **Real `kill -9` crash-recovery integration test** (`tests/crash_recovery.rs`): spawns
  the actual `evald serve` binary, ingests over OTLP until spans are durably ACK'd, waits
  for a compaction to commit cold Parquet, ingests more (left in the hot tier/WAL), then
  SIGKILLs the process mid-flight and verifies on restart that every ACK'd span returns
  exactly once — no loss, no double-count. Uses only the std lib + serde_json (no
  HTTP-client dependency).
- Release-gating files for the first public OSS mirror: `LICENSE` (Apache-2.0),
  `NOTICE` (third-party attribution), `SECURITY.md`, `CONTRIBUTING.md`,
  `CODE_OF_CONDUCT.md`, and this changelog.
- **CI/packaging pipeline** for the public mirror: a `release` workflow (tag-driven)
  that builds static-musl (x86_64/aarch64) + macOS + Windows binaries, publishes a
  GitHub release with `SHA256SUMS`, publishes the crate to crates.io, bumps the
  Homebrew formula, and pushes a multi-arch distroless image; plus a `ci` workflow
  (fmt/clippy/test + static-musl build). `Dockerfile` (from-source) +
  `Dockerfile.release` (prebuilt binaries) + `.dockerignore`, `RELEASING.md`,
  `docs/DOCKERHUB.md`, and `cargo-binstall` metadata. Distribution channels:
  `cargo binstall evald`, `cargo install evald`, `brew install evald`,
  `docker run mancube/evald`.
- Five more deterministic Tier-1 evaluators (zero-cost, no network, no user code),
  rounding out the offline scorer set: `non_empty`, `contains_all` (partial-credit
  over a keyword list), `contains_any`, `length_bounds` (char-count min/max), and
  `numeric_tolerance` (absolute tolerance over numeric outputs; non-numeric inputs
  are skipped, not failed).
- **Statistical significance for `eval compare`** — `--significance [--alpha 0.05]` gates
  CI on a regression only when it is more than sampling noise. The aggregate Score now
  carries per-run sufficient statistics (n, pass_count, mean, variance); compare runs a
  dependency-free **Welch's two-sample t-test** and reports the p-value, the `(1 − α)`
  confidence interval on the delta, and a `signif`/`noise` verdict. The significance gate
  forgives a drop only when the test proves it is within noise; an untestable regression
  (a run predating the stats field, or n < 2) still gates. The default `--fail-on-regression`
  behavior (raw delta vs `--tolerance`) is unchanged. New `stats` module: regularized
  incomplete beta + Student's-t CDF/quantile, no `statrs`/`nalgebra` dependency. The new
  `Score.agg_stats` field is `Option` + serde-default, so the on-disk format stays
  backward/forward compatible.

### Added — core engine (store, ingest, query, eval, SPA)
- **OTLP/HTTP receiver** on `:4318` (`POST /v1/traces`) accepting protobuf
  (gzip-aware) and **OTLP-JSON**, decoded via `opentelemetry-proto` wire types.
- **Span normalization** unifying the OpenInference (`openinference.span.kind`)
  and OTel `gen_ai.*` conventions into one model: dialect auto-detect,
  model/provider, tokens mapped both directions (incl. cache-creation →
  cache_write), cost (price table fallback), I/O capture, lossless
  `raw_attributes`.
- **Durable two-tier store**: a fsynced write-ahead log as the ACK boundary → a
  background compactor flushing sealed segments to **time-partitioned Parquet**
  (Snappy), with a redb `trace_id`→block index + a compaction watermark. The
  hot→cold commit protocol is crash-safe (verified with `kill -9` during
  compaction). Overload sheds with `429 + Retry-After` — never a silent drop.
- **Read API**: `GET /v1/spans` (`?trace_id=&limit=`) and
  `GET /v1/traces/{trace_id}`, unioning hot ∪ cold and deduping by
  `(trace_id, span_id)`.
- **DataFusion SQL** over the Parquet blocks ∪ hot tier ∪ scores
  (`POST /v1/sql` + `evald query`); read statements only.
- **Universal Score store** (redb): `POST`/`GET /v1/scores`,
  `GET /v1/scores/{id}`, and a Phoenix-compatible `POST /v1/span_annotations`
  (`{"data":[…]}` envelope).
- **Offline eval-regression runner** — the core of evald: `evald eval run` scores a JSONL
  dataset with deterministic Tier-1 evaluators (`exact_match`, `contains`,
  `regex`, `json_valid`, `levenshtein`), persists per-item + aggregate Scores, and
  **exits non-zero on a threshold regression** (a CI gate).
- **`evald eval compare <runA> <runB>`** — diffs two runs' aggregates by
  evaluator; `--fail-on-regression [--tolerance]` is a second CI gate.
- **Embedded SPA** (rust-embed): trace list → span tree → scores + a SQL console,
  served at `/`, works air-gapped.

> Pre-1.0: on-disk formats and the HTTP/CLI API may change between minor versions
> until the GA format-freeze (see `PLAN.md`).
