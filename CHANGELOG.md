# Changelog

All notable changes to evald are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project aims to
follow [Semantic Versioning](https://semver.org/).

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
  no Enterprise view can re-enter the OSS bundle.

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
- **Tier-3 LLM-as-judge** (`judges:` in the eval config) — the eval differentiator. Declarative,
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
