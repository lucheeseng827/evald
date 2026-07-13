//! evald — embedded OTel-native trace + eval store for LLM apps (single binary).
//!
//! STATUS: PoC, build steps 1–8 of PLAN.md §4. The [`ingest`] front door is real — an
//! OTLP/HTTP `POST /v1/traces` receiver that decodes both protobuf and OTLP-JSON into
//! the opentelemetry-proto wire types, runs them through [`normalize`] into a
//! [`NormalizedSpan`] that unifies the OpenInference and `gen_ai.*` conventions, and
//! durably persists them via [`store`]: a WAL-as-ACK-boundary feeding a background
//! compactor that flushes to time-partitioned Parquet under a crash-safe commit protocol
//! (redb index + watermark), with hot ∪ cold dedup on read. Kill-9-recoverable, verified.
//! The universal [`Score`] object is persisted too (redb), with `/v1/scores` and a
//! Phoenix-compatible `/v1/span_annotations`. The offline [`eval`] runner is live —
//! `evald eval run` scores a JSONL dataset with Tier-1 deterministic evaluators and gates
//! CI on thresholds, and `evald eval compare` reads two runs' aggregate Scores back and
//! diffs them by evaluator (with a `--fail-on-regression` run-vs-run CI gate). [`sql`]
//! points **DataFusion** at the cold Parquet blocks for ad-hoc SQL (`POST /v1/sql`,
//! `evald query`), and [`ui`] is the embedded SPA (rust-embed) served with SPA fallback.
//! See `PLAN.md` for the design, commit protocol, and build order.
//!
//! # Planned module layout (per PLAN.md §3; not yet split into crates)
//!
//! - **ingest** — OTLP/HTTP `:4318` receiver (gRPC `:4317` in Beta). Decodes
//!   protobuf + OTLP-JSON (hex ids, int enums, int64-as-string, lowerCamelCase) and
//!   normalizes OpenInference vs `gen_ai.*` into one [`NormalizedSpan`]. The bounded
//!   channel applies backpressure and sheds with `429/503 + Retry-After`; it never
//!   silently drops. The WAL append is the ACK boundary, decoupling client latency
//!   from hot-tier compaction stalls.
//! - **store** — durable WAL (the ACK boundary) -> hot tier (recent spans) ->
//!   background compaction to time-partitioned Parquet, with a `redb` index that holds
//!   `trace_id -> partition` AND the compaction high-water-mark. Recovery replays
//!   everything above the watermark; queries dedup by `(trace_id, span_id)` across the
//!   hot/cold boundary so an in-flight compaction never surfaces duplicates or gaps.
//!   The hot-tier engine is chosen by a soak benchmark, not assumed (PLAN.md §6).
//! - **eval** — the [`Score`]-as-universal-object model and the offline
//!   dataset/experiment **regression** runner (`eval run` / `eval compare`, CI exit
//!   codes). MVP ships a FIXED built-in Tier-1 deterministic scorer set only — no
//!   user shell/wasm code. Because evald is OTel-native, `span_id`/`trace_id` IS the
//!   join key, so online/offline/human scores share one storage path and one schema.
//! - **api** — axum routes (`/v1/traces`, `/v1/spans`, `/v1/scores`,
//!   `/v1/span_annotations` with the Phoenix `{ "data": [ ... ] }` envelope,
//!   `/v1/runs/{id}/compare`).
//! - **ui** — an embedded SPA (rust-embed) served with SPA fallback.

pub mod auth;
pub mod blob;
pub mod calibrate;
pub mod cost;
pub mod dataset;
pub mod eval;
pub mod grpc;
pub mod ingest;
pub mod judge;
pub mod model;
pub mod normalize;
pub mod sql;
pub mod stats;
pub mod store;
pub mod suite;
pub mod ui;

pub use auth::{Auth, AuthError};
pub use blob::BlobStore;
pub use model::{
    AggStats, DataType, Dialect, NormalizedSpan, Score, ScoreSource, ScoreTarget, Tokens,
    UsageMissingReason,
};
pub use store::{IngestStats, ReclaimReport, Store, StoreConfig, StoreError};

/// Initialize the tracing subscriber on **stderr** (honors `RUST_LOG`, defaults to
/// `info`). Logs go to stderr so a command's real output (e.g. `eval run`'s report)
/// stays on stdout and parseable in CI.
///
/// Idempotent: a no-op if a subscriber is already installed, so it is safe to call
/// from the binary and harmless under tests.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Version helper surfaced by `evald version`.
#[derive(Debug, Clone, Copy)]
pub struct Version;

impl Version {
    /// The crate version, stamped at build time.
    pub const fn current() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
}
