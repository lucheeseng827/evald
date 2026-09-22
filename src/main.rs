//! evald — single-binary CLI entrypoint.
//!
//! Status: PoC, build steps 1–8. `serve` runs the OTLP/HTTP receiver + durable store
//! (WAL + compaction to Parquet) + query/SQL API + embedded SPA; `eval run` scores a JSONL
//! dataset and gates CI on thresholds, `eval compare` diffs two runs' aggregate scores with
//! a regression gate, and `query` runs DataFusion SQL over the cold blocks. The shape
//! mirrors the planned UX: headless-first, CI-friendly exit codes.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Parser, Subcommand};
use evald::StoreConfig;

/// evald — embedded OTel-native trace + eval store for LLM apps (single binary).
#[derive(Parser)]
#[command(
    name = "evald",
    version,
    about = "Embedded OTel-native trace + eval store for LLM apps (single binary)."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

// `Serve` carries every server flag and so dwarfs the one-shot variants. Boxing it is the
// usual fix and is not available here: clap's derive needs the variant's fields inline to
// build the argument parser. The cost the lint exists to prevent — many values of an
// oversized enum — cannot arise: exactly one `Cmd` is parsed per process, matched once, and
// dropped.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Run the store: OTLP/HTTP receiver + query API (+ eval API + embedded UI, later).
    Serve {
        /// Address for the OTLP/HTTP receiver (`POST /v1/traces`). Defaults to the
        /// loopback OTLP/HTTP port so it never listens on the network unasked.
        #[arg(long, default_value = "127.0.0.1:4318", env = "EVALD_OTLP_HTTP_ADDR")]
        otlp_http: String,
        /// Address for the OTLP/gRPC receiver (:4317). Empty disables gRPC ingest (HTTP only).
        /// Defaults to the loopback OTLP/gRPC port so it never listens on the network unasked.
        #[arg(long, default_value = "127.0.0.1:4317", env = "EVALD_OTLP_GRPC_ADDR")]
        otlp_grpc: String,
        /// Directory for durable storage (write-ahead log + Parquet blocks + index).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Seal the active WAL segment after this many spans (then it compacts to Parquet).
        #[arg(long, default_value_t = 50_000, env = "EVALD_SEAL_THRESHOLD")]
        seal_threshold: usize,
        /// Background compaction interval, in seconds (0 disables background compaction).
        #[arg(long, default_value_t = 5, env = "EVALD_COMPACT_INTERVAL_SECS")]
        compact_interval_secs: u64,
        /// Cold-to-cold compaction: merge an hour's Parquet blocks once it holds this many.
        /// Every sealed segment becomes a block, so without merging the block count only
        /// grows — and a full scan's open-file working set with it, until queries fail with
        /// `Too many open files`. `0` turns merging off (`evald compact` still merges on
        /// demand). See `docs/FORMAT.md`.
        #[arg(long, default_value_t = 16, env = "EVALD_COLD_MERGE_THRESHOLD")]
        cold_merge_threshold: usize,
        /// A merged block never exceeds this many spans; a partition with more becomes
        /// several blocks. Keeps retention (which drops blocks whole) and merge memory
        /// granular.
        #[arg(long, default_value_t = 1_000_000, env = "EVALD_COLD_MERGE_MAX_SPANS")]
        cold_merge_max_spans: usize,
        /// Also collapse each fully-past UTC day into day blocks, so a low-volume store's
        /// block count grows per day rather than per hour. `--cold-merge-days false` keeps
        /// merging inside the hour partitions.
        #[arg(long, default_value_t = true, action = ArgAction::Set, env = "EVALD_COLD_MERGE_DAYS")]
        cold_merge_days: bool,
        /// A closed day is only collapsed once nothing has been written into it for this
        /// many seconds, so a backfill (or a client with a skewed clock) does not have the
        /// day rewritten on every tick. `0` collapses it on the first tick after midnight.
        #[arg(long, default_value_t = 3600, env = "EVALD_COLD_MERGE_DAY_QUIET_SECS")]
        cold_merge_day_quiet_secs: u64,
        /// How many seconds a merge input stays on disk after it leaves the index, so a
        /// query that listed it moments before the merge can still read it. Raise it above
        /// your slowest query if a long scan ever races a merge.
        #[arg(long, default_value_t = 60, env = "EVALD_COLD_MERGE_GRACE_SECS")]
        cold_merge_grace_secs: u64,
        /// Durable-backlog bound: shed ingest (429 + Retry-After) once this many un-compacted
        /// spans are resident in memory. Everything accepted is already durable in the WAL, so
        /// this bounds memory, not durability — it keeps a lagging compactor from OOMing the
        /// process. `0` disables the bound (unbounded hot tier).
        ///
        /// The default is ~560 MiB of spans at a 1 KiB payload; the whole process peaks
        /// around 852 MiB at that bound, which is what the shipped 1 GiB container limit is
        /// sized for. Size it against YOUR limit and payload — `docs/CONFIG.md` has the
        /// arithmetic.
        #[arg(long, default_value_t = 300_000, env = "EVALD_MAX_HOT_SPANS")]
        max_hot_spans: usize,
        /// Offload a span input/output payload larger than this many bytes to the blob store,
        /// leaving a compact `evald-blob:<key>` reference on the span (fetch via
        /// `GET /v1/blobs/{key}`). Keeps a megabyte RAG context or tool output out of the WAL /
        /// Parquet / query response. `0` disables offloading (payloads stay inline).
        #[arg(long, default_value_t = 256 * 1024, env = "EVALD_BLOB_OFFLOAD_BYTES")]
        blob_offload_bytes: usize,
        /// Require an `Authorization: Bearer <token>` on every request (HTTP + gRPC). Each
        /// occurrence is ONE whole token (commas allowed); repeat the flag for several accepted
        /// tokens (rotation / per-client revocation). The env var `EVALD_AUTH_TOKEN` is read as
        /// a comma-separated list and UNIONED with these (and with `--auth-token-file`). Tokens
        /// must be at least 16 printable-ASCII chars. With nothing configured, authentication is
        /// OFF — the default local posture (`serve` also binds loopback by default). Turn this
        /// on before exposing evald on a shared or public network. This is a shared-secret gate,
        /// not TLS — terminate TLS at a reverse proxy if the network is untrusted.
        #[arg(long = "auth-token")]
        auth_token: Vec<String>,
        /// A file of bearer tokens — one per line; blank lines and `#` comments ignored —
        /// unioned with any `--auth-token` values. Keeps secrets out of argv/env and lets you
        /// rotate by editing the file. See `--auth-token`.
        #[arg(long = "auth-token-file", env = "EVALD_AUTH_TOKEN_FILE")]
        auth_token_file: Option<PathBuf>,
        /// Automatically drop cold blocks whose spans ALL predate `now − <window>`, on a
        /// timer. Suffixes: `d`/`h`/`m`/`s`; a bare number is seconds. UNSET = no automatic
        /// deletion — evald never removes a user's data unless asked. `evald retention`
        /// remains available for a one-shot sweep (with `--dry-run`).
        #[arg(long, value_parser = parse_retention, env = "EVALD_RETENTION")]
        retention: Option<Duration>,
        /// How often the automatic retention sweep runs, in seconds.
        #[arg(long, default_value_t = 3600, env = "EVALD_RETENTION_INTERVAL_SECS")]
        retention_interval_secs: u64,
        /// Disk floor: refuse ingest (503 + Retry-After) while the data-dir filesystem has
        /// less than this free. Accepts `512MiB`, `2g`, or a bare byte count. `0` disables
        /// the floor. Defaults to 256MiB — running a volume to zero is how the store wedges
        /// mid-write, and this leaves room to finish an in-flight flush and investigate.
        #[arg(long, value_parser = evald::disk::parse_bytes, default_value = "256MiB", env = "EVALD_DISK_MIN_FREE")]
        disk_min_free: u64,
        /// Warn (log + `evald_disk_blocked` stays 0) once free space falls below this.
        /// `0` disables the warning. Should be comfortably above `--disk-min-free`.
        #[arg(long, value_parser = evald::disk::parse_bytes, default_value = "1GiB", env = "EVALD_DISK_WARN_FREE")]
        disk_warn_free: u64,
        /// How often free space is sampled, in seconds. `0` disables the disk guardrail
        /// entirely — no probe, no floor, pre-guardrail ingest behaviour.
        #[arg(long, default_value_t = 10, env = "EVALD_DISK_CHECK_INTERVAL_SECS")]
        disk_check_interval_secs: u64,
        /// Redact sensitive values from prompts, completions and span attributes BEFORE
        /// anything is written. Comma-separated classes, or `all`:
        /// `email`, `credit_card` (Luhn-checked), `ssn`, `phone`, `ip`, `api_key`, `jwt`.
        /// Repeatable. UNSET = no redaction (the default — this rewrite is irreversible,
        /// since the raw value never reaches disk, so it is always an explicit choice).
        #[arg(long = "redact", env = "EVALD_REDACT")]
        redact: Vec<String>,
        /// What to do with a detected value: `redact` → `[REDACTED:<class>]`; `hash` →
        /// `[<class>:<hash>]`, unrecoverable but stable, so equal values stay groupable;
        /// `drop` → removed entirely.
        #[arg(long, default_value = "redact", env = "EVALD_REDACT_ACTION")]
        redact_action: String,
        /// An extra redaction rule as `name=regex`, e.g. `employee_id=EMP-[0-9]{6}`.
        /// Repeatable. Compiled at startup, so a bad pattern fails the process rather than
        /// silently never matching once traffic is flowing.
        #[arg(long = "redact-custom")]
        redact_custom: Vec<String>,
        /// How a score name combines when `GET /v1/traces/{id}/scores` rolls span scores up
        /// into a trace-level value: `name=fn`, where fn is
        /// `mean` | `min` | `max` | `sum` | `all` | `any`. Repeatable, comma-separated.
        /// Defaults: `mean` for numeric scores, `all` for boolean. A CI gate usually wants
        /// `min` — one bad step in a ten-step agent should fail the trace, and a mean
        /// dilutes it. Read-time only: nothing is rewritten, and changing this changes the
        /// answer for traces already stored.
        #[arg(long = "rollup", env = "EVALD_ROLLUP")]
        rollup: Vec<String>,
        /// A model price table (the LiteLLM `model_prices_and_context_window.json` shape, or any
        /// subset of it) laid over the built-in one: it wins for every model it names. It prices
        /// spans that carry a model and token counts but no cost of their own, at ingest. The
        /// file is re-read when it changes; a bad edit keeps the previous table. No network
        /// access: keep the file fresh yourself.
        #[arg(long = "price-table", env = "EVALD_PRICE_TABLE")]
        price_table: Option<PathBuf>,
        /// Do not record the LLM usage series (cost, tokens, latency, rolling eval scores) on
        /// `/metrics`. On by default: they cost one lock per committed batch and a few hundred
        /// series at most. evald's own health series are unaffected.
        #[arg(long, env = "EVALD_NO_USAGE_METRICS")]
        no_usage_metrics: bool,
        /// Distinct model names carried as a label on the usage series before the rest are
        /// folded into `other` (counted by `evald_usage_labels_folded_total`). Bounds the
        /// series count, and so the scrape size and the scraper's memory.
        #[arg(long, default_value_t = 100, env = "EVALD_METRICS_MODEL_CAP")]
        metrics_model_cap: usize,
    },
    /// Eval runner subcommands (offline regression loop).
    Eval {
        #[command(subcommand)]
        action: EvalCmd,
    },
    /// Token-cost attribution report — group the per-span cost + tokens by a dimension.
    Cost {
        /// Attribution dimension: model | user | session | service | provider.
        #[arg(long, default_value = "model")]
        by: String,
        /// Directory for durable storage (the spans to report over).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Max attribution rows to print.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Re-price the report from the stored token counts under this table (laid over the
        /// built-in one) instead of showing the costs stored at ingest. Nothing is rewritten.
        #[arg(long = "price-table", env = "EVALD_PRICE_TABLE")]
        price_table: Option<PathBuf>,
    },
    /// Latency percentiles (exact nearest-rank p50/p95/p99) and time-to-first-token over LLM
    /// spans, grouped by a dimension.
    Latency {
        /// Grouping dimension: model | provider | service.
        #[arg(long, default_value = "model")]
        by: String,
        /// Directory for durable storage (the spans to report over).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Max groups to print.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Export stored scores in another shape.
    Scores {
        #[command(subcommand)]
        action: ScoresCmd,
    },
    /// Curate an eval dataset from captured spans (the production→eval loop).
    Dataset {
        #[command(subcommand)]
        action: DatasetCmd,
    },
    /// Run a declarative eval SUITE (promptfoo-style): many eval cases, one suite pass-gate.
    Suite {
        #[command(subcommand)]
        action: SuiteCmd,
    },
    /// Run a read-only SQL query over the stored spans + scores (DataFusion).
    Query {
        /// The SQL to run. Tables: `spans` (hot ∪ cold, deduped) and `scores`.
        sql: String,
        /// Directory for durable storage (the blocks + score store to query).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Max rows to print.
        #[arg(long, default_value_t = 1_000)]
        limit: usize,
    },
    /// Reclaim disk by dropping whole time-partitioned Parquet blocks older than a retention
    /// window. Index entry + file are unlinked (`O(unlink)`, space-reclaiming — no row-by-row
    /// DELETE, no vacuum); the hot tier is never touched.
    Retention {
        /// Drop blocks whose spans ALL predate `now − <window>`. Suffixes: `d` days, `h` hours,
        /// `m` minutes, `s` seconds; a bare number is seconds. E.g. `30d`, `72h`, `90m`.
        #[arg(long, value_parser = parse_retention)]
        older_than: Duration,
        /// Directory for durable storage (the blocks to sweep).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Preview only: report what WOULD be reclaimed without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Merge cold Parquet blocks (cold-to-cold compaction): collapse every hour partition
    /// holding more than one block, then every fully-past UTC day. Bounds the block count a
    /// query has to open, which is what keeps a long-lived store queryable. Safe to run
    /// while nothing else has the data-dir open; the server does the same work on its
    /// compaction tick.
    Compact {
        /// Directory for durable storage (the blocks to merge).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Preview only: report what WOULD be merged without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// A merged block never exceeds this many spans.
        #[arg(long, default_value_t = 1_000_000, env = "EVALD_COLD_MERGE_MAX_SPANS")]
        max_spans: usize,
        /// Also collapse fully-past UTC days into day blocks.
        #[arg(long, default_value_t = true, action = ArgAction::Set, env = "EVALD_COLD_MERGE_DAYS")]
        days: bool,
    },
    /// Report (and, by default, apply) what this build needs to do to a data-dir written by
    /// an older evald: stamp the on-disk format marker. Exits non-zero if the directory was
    /// written by a NEWER format than this build understands — see `docs/FORMAT.md`.
    Migrate {
        /// Directory for durable storage to inspect.
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Report what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the evald version.
    Version,
}

/// Parse a retention window (`30d` / `72h` / `90m` / `3600s`, or a bare number = seconds) into a
/// [`Duration`]. Rejects empty / non-numeric / overflowing values with a clear message.
fn parse_retention(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit_secs): (&str, u64) = match s.chars().last() {
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some('h') => (&s[..s.len() - 1], 3_600),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('s') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    let value: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid retention window {s:?} (use e.g. 30d, 72h, 90m, 3600s)"))?;
    value
        .checked_mul(unit_secs)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("retention window {s:?} is too large"))
}

#[derive(Subcommand)]
enum SuiteCmd {
    /// Run a suite YAML (cases + suite-level pass gate); exits non-zero if the suite fails (CI gate).
    Run {
        /// Path to the suite config (cases[] + min_pass_rate / repeat / min_pass).
        #[arg(long, default_value = "suite.yaml")]
        config: PathBuf,
        /// Also write a JUnit XML report here (written even when the suite fails, so CI test
        /// tabs can show which case failed; the exit code is unchanged).
        #[arg(long, value_name = "PATH")]
        junit: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ScoresCmd {
    /// Print stored scores as `gen_ai.evaluation.result` events, one JSON object per line
    /// (oldest first). Only scores that belong to a span or trace are exported; run aggregates
    /// and session scores have no operation to attach an event to. Takes the data-dir lock, so
    /// it cannot run against a live `serve` on the same directory.
    Export {
        /// Output shape. Only `gen_ai-event` today.
        #[arg(long, default_value = "gen_ai-event")]
        format: String,
        /// Only scores with this name.
        #[arg(long)]
        name: Option<String>,
        /// Export at most this many (the newest).
        #[arg(long, default_value_t = 10_000)]
        limit: usize,
        /// Directory for durable storage (the scores are read from here).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum DatasetCmd {
    /// Select stored spans into a JSONL dataset compatible with `eval run` (only spans that carry a
    /// captured `output.value` become rows; provenance span/trace ids + metadata are attached).
    FromSpans {
        /// Directory for durable storage (the spans to curate).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Only curate spans of this trace (default: all recent spans).
        #[arg(long)]
        trace_id: Option<String>,
        /// Only curate spans of this model.
        #[arg(long)]
        model: Option<String>,
        /// Max spans to scan, most-recent-first.
        #[arg(long, default_value_t = 1_000)]
        limit: usize,
        /// Output JSONL path (writes to stdout when omitted).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum EvalCmd {
    /// Run an eval from a YAML config (exit-nonzero on a threshold regression — CI).
    Run {
        /// Path to the eval config (dataset + evaluators[] + thresholds).
        #[arg(long, default_value = "eval.yaml")]
        config: PathBuf,
        /// Directory for durable storage (scores are persisted here).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Preview the judge token usage + cost for this config WITHOUT calling any provider
        /// (no scoring, no network) — then exit. Tier-1 scorers are zero-cost.
        #[arg(long)]
        estimate: bool,
        /// Also write a JUnit XML report here, one test case per evaluator (written even when a
        /// threshold fails or the run errors; the exit code is unchanged).
        #[arg(long, value_name = "PATH")]
        junit: Option<PathBuf>,
    },
    /// Diff two runs' aggregate scores by evaluator (mean delta + regression gate).
    Compare {
        /// Baseline run id (run A).
        run_a: String,
        /// Candidate run id (run B) — the diff is `B - A` per evaluator.
        run_b: String,
        /// Directory for durable storage (the runs' scores are read from here).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Exit non-zero if any shared evaluator regressed beyond `--tolerance` (CI gate).
        #[arg(long)]
        fail_on_regression: bool,
        /// How far an evaluator may drop (B below A) before it counts as a regression.
        #[arg(long, default_value_t = 0.0, value_parser = parse_tolerance)]
        tolerance: f64,
        /// Gate only on *statistically significant* regressions (Welch's t): a drop beyond
        /// `--tolerance` fails the build only when the `(1 - --alpha)` confidence interval
        /// proves it is more than sampling noise. Without this, `--fail-on-regression` gates
        /// on the raw delta. Always prints the p-value + CI when the runs carry stats.
        #[arg(long)]
        significance: bool,
        /// Significance level for `--significance` and the reported CI (e.g. 0.05 → 95% CI).
        #[arg(long, default_value_t = 0.05, value_parser = parse_alpha)]
        alpha: f64,
        /// Also write a JUnit XML report here, one test case per evaluator delta (a case fails
        /// exactly when the gate fails; written even then, and the exit code is unchanged).
        #[arg(long, value_name = "PATH")]
        junit: Option<PathBuf>,
    },
    /// Calibrate an LLM-as-judge against human annotations on the same spans (bias + agreement).
    Calibrate {
        /// Judge score name to calibrate (e.g. `judge_g_eval`) — paired with human annotations
        /// on the same span/trace.
        #[arg(long)]
        judge: String,
        /// Only pair against human annotations with this name (default: any human score).
        #[arg(long)]
        human: Option<String>,
        /// Directory for durable storage (the scores are read from here).
        #[arg(long, default_value = "evald-data", env = "EVALD_DATA_DIR")]
        data_dir: PathBuf,
        /// Significance level for the reported confidence interval on the bias (e.g. 0.05 → 95%).
        #[arg(long, default_value_t = 0.05, value_parser = parse_alpha)]
        alpha: f64,
        /// How far (in score units) the judge may drift from the human before it is flagged for
        /// recalibration (MAE, or a significant bias, beyond this).
        #[arg(long, default_value_t = 0.2, value_parser = parse_tolerance)]
        threshold: f64,
        /// Exit non-zero when the judge needs recalibration (CI gate against judge drift).
        #[arg(long)]
        fail_on_divergence: bool,
    },
}

/// Parse `--tolerance`, rejecting negative / non-finite values. A negative tolerance would
/// invert the regression check (`delta < -tolerance`) and flag small *improvements* as
/// regressions, so we fail fast with a clear message instead.
fn parse_tolerance(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!(
            "--tolerance must be a finite value >= 0.0 (got {s:?})"
        ));
    }
    Ok(v)
}

/// Parse `--alpha`, the significance level. Must be strictly in (0, 1) — `0` or `1` would make
/// the confidence interval degenerate and the gate meaningless.
fn parse_alpha(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if !v.is_finite() || v <= 0.0 || v >= 1.0 {
        return Err(format!(
            "--alpha must be in the open interval (0, 1) (got {s:?})"
        ));
    }
    Ok(v)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Serve {
            otlp_http,
            otlp_grpc,
            data_dir,
            seal_threshold,
            compact_interval_secs,
            cold_merge_threshold,
            cold_merge_max_spans,
            cold_merge_days,
            cold_merge_day_quiet_secs,
            cold_merge_grace_secs,
            max_hot_spans,
            blob_offload_bytes,
            auth_token,
            auth_token_file,
            retention,
            retention_interval_secs,
            disk_min_free,
            disk_warn_free,
            disk_check_interval_secs,
            redact,
            redact_action,
            redact_custom,
            rollup,
            price_table,
            no_usage_metrics,
            metrics_model_cap,
        } => {
            evald::init_tracing();
            // `EVALD_AUTH_TOKEN` is read HERE, not via a clap `env` fallback, so it UNIONS with
            // any `--auth-token` flags and `--auth-token-file` (clap would instead let a single
            // `--auth-token` *replace* the env value, silently dropping it — breaking rotation —
            // and would turn an empty `EVALD_AUTH_TOKEN=` placeholder into a bogus token). It is
            // a comma-separated list; unset or empty contributes nothing.
            let mut auth_tokens = auth_token;
            if let Ok(env_tokens) = std::env::var("EVALD_AUTH_TOKEN") {
                auth_tokens.extend(env_tokens.split(',').map(str::to_string));
            }
            // Assemble the bearer-token gate before binding: a bad token config (too short,
            // non-ASCII, an unreadable file, or auth-requested-but-empty) fails here with a clear
            // message instead of after the listener is up. Nothing configured → auth disabled.
            let auth = evald::Auth::from_sources(&auth_tokens, auth_token_file.as_deref())
                .map_err(|e| anyhow::anyhow!("auth configuration error: {e}"))?;
            let addr: SocketAddr = otlp_http
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid --otlp-http address {otlp_http:?}: {e}"))?;
            // Empty --otlp-grpc disables the gRPC listener (HTTP-only).
            let grpc_addr: Option<SocketAddr> = match otlp_grpc.trim() {
                "" => None,
                s => Some(
                    s.parse()
                        .map_err(|e| anyhow::anyhow!("invalid --otlp-grpc address {s:?}: {e}"))?,
                ),
            };
            // Compiled up front: a malformed class, action or custom regex must stop the
            // process here, not be discovered as "redaction silently did nothing" later.
            let redactor = evald::redact::Redactor::build(&redact, &redact_custom, &redact_action)
                .map_err(|e| anyhow::anyhow!("redaction config: {e}"))?;
            if let Some(r) = &redactor {
                tracing::info!(
                    rules = ?r.labels(),
                    "redaction ARMED — matched values are rewritten before the WAL, irreversibly"
                );
            }
            let rollup = evald::rollup::RollupConfig::parse(&rollup)
                .map_err(|e| anyhow::anyhow!("rollup config: {e}"))?;
            // A price table that does not parse stops the process here, like the other configs.
            evald::price::install(price_table.as_deref())
                .map_err(|e| anyhow::anyhow!("price table: {e:#}"))?;
            let config = StoreConfig {
                seal_threshold_spans: seal_threshold,
                compact_interval: (compact_interval_secs > 0)
                    .then(|| Duration::from_secs(compact_interval_secs)),
                max_hot_spans,
                blob_offload_bytes,
                retention,
                retention_interval: Duration::from_secs(retention_interval_secs.max(1)),
                disk_min_free_bytes: disk_min_free,
                disk_warn_free_bytes: disk_warn_free,
                disk_check_interval: (disk_check_interval_secs > 0)
                    .then(|| Duration::from_secs(disk_check_interval_secs)),
                redactor,
                rollup,
                usage: evald::usage::UsageConfig {
                    enabled: !no_usage_metrics,
                    model_cap: metrics_model_cap,
                },
                merge: evald::store::MergePolicy {
                    hour_threshold: cold_merge_threshold,
                    max_spans_per_block: cold_merge_max_spans,
                    day_merge: cold_merge_days,
                    day_quiet: Duration::from_secs(cold_merge_day_quiet_secs),
                    unlink_grace: Duration::from_secs(cold_merge_grace_secs),
                },
                ..StoreConfig::default()
            };
            // A floor at or above the warning makes the warning unreachable — the operator
            // would get the hard stop with no prior signal, which is the opposite of what
            // they configured. Refuse at startup rather than silently misbehave.
            //
            // Only while the guardrail is actually running, though: `--disk-check-interval-secs
            // 0` turns the probe off, and then neither threshold is ever read. Refusing to boot
            // over two inert numbers would invent an outage in exactly the configuration that
            // asked for no guardrail at all — a chart or unit file that sets the thresholds from
            // one template and disables the probe separately is a normal way to reach it. Say it
            // once, at WARN, so the misconfiguration is not silent if the probe is turned back on.
            if disk_warn_free != 0 && disk_min_free != 0 && disk_warn_free <= disk_min_free {
                if disk_check_interval_secs > 0 {
                    anyhow::bail!(
                        "--disk-warn-free ({}) must be greater than --disk-min-free ({}) — \
                         otherwise the warning can never fire before the floor",
                        evald::disk::human_bytes(disk_warn_free),
                        evald::disk::human_bytes(disk_min_free)
                    );
                }
                tracing::warn!(
                    warn_free = %evald::disk::human_bytes(disk_warn_free),
                    min_free = %evald::disk::human_bytes(disk_min_free),
                    "--disk-warn-free is not above --disk-min-free; harmless while \
                     --disk-check-interval-secs is 0, but the warning could never fire if the \
                     disk guardrail were enabled"
                );
            }
            evald::ingest::serve(addr, grpc_addr, data_dir, config, auth).await?;
        }
        Cmd::Eval { action } => match action {
            EvalCmd::Run {
                config,
                data_dir,
                estimate,
                junit,
            } => {
                evald::init_tracing();
                if estimate {
                    evald::eval::estimate_command(&config, &data_dir)?;
                } else {
                    let passed =
                        evald::eval::run_command_junit(&config, &data_dir, junit.as_deref())
                            .await?;
                    if !passed {
                        // CI gate: a regressed threshold exits non-zero.
                        std::process::exit(1);
                    }
                }
            }
            EvalCmd::Compare {
                run_a,
                run_b,
                data_dir,
                fail_on_regression,
                tolerance,
                significance,
                alpha,
                junit,
            } => {
                evald::init_tracing();
                let passed = evald::eval::compare_command_junit(
                    &run_a,
                    &run_b,
                    &data_dir,
                    evald::eval::CompareOpts {
                        fail_on_regression,
                        tolerance,
                        significance,
                        alpha,
                    },
                    junit.as_deref(),
                )
                .await?;
                if !passed {
                    // CI gate: a regressed evaluator (with --fail-on-regression) exits non-zero.
                    std::process::exit(1);
                }
            }
            EvalCmd::Calibrate {
                judge,
                human,
                data_dir,
                alpha,
                threshold,
                fail_on_divergence,
            } => {
                evald::init_tracing();
                let passed = evald::calibrate::calibrate_command(
                    &judge,
                    human.as_deref(),
                    &data_dir,
                    alpha,
                    threshold,
                    fail_on_divergence,
                )?;
                if !passed {
                    // CI gate: judge has drifted from the human ground truth (--fail-on-divergence).
                    std::process::exit(1);
                }
            }
        },
        Cmd::Cost {
            by,
            data_dir,
            limit,
            price_table,
        } => {
            evald::init_tracing();
            let dimension = evald::cost::Dimension::parse(&by)?;
            evald::cost::cost_command(dimension, &data_dir, limit, price_table.as_deref()).await?;
        }
        Cmd::Latency {
            by,
            data_dir,
            limit,
        } => {
            evald::init_tracing();
            let dimension = evald::latency::Dimension::parse(&by)?;
            evald::latency::latency_command(dimension, &data_dir, limit).await?;
        }
        Cmd::Suite { action } => match action {
            SuiteCmd::Run { config, junit } => {
                evald::init_tracing();
                let started = std::time::Instant::now();
                let result = evald::suite::run_suite(&config);
                let suite_name = config
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| config.display().to_string());
                evald::junit::write_after(
                    junit.as_deref(),
                    &result,
                    |o| evald::junit::from_suite(o, &suite_name),
                    &format!("evald suite {suite_name}"),
                    "evald.suite",
                    started.elapsed().as_secs_f64(),
                )?;
                let outcome = result?;
                for c in &outcome.cases {
                    let mark = if c.passed { "PASS" } else { "FAIL" };
                    println!(
                        "  [{mark}] {} ({}/{} runs passed)",
                        c.name, c.runs_passed, c.runs_total
                    );
                }
                println!(
                    "suite: {:.0}% of cases passed ({})",
                    outcome.pass_rate * 100.0,
                    if outcome.passed { "PASS" } else { "FAIL" }
                );
                if !outcome.passed {
                    std::process::exit(1);
                }
            }
        },
        Cmd::Scores { action } => match action {
            ScoresCmd::Export {
                format,
                name,
                limit,
                data_dir,
            } => {
                anyhow::ensure!(
                    format == "gen_ai-event",
                    "unknown --format {format:?} (expected: gen_ai-event)"
                );
                evald::init_tracing();
                let n = evald::evalevent::export_command(&data_dir, name.as_deref(), limit)?;
                eprintln!("evald scores export: wrote {n} event(s)");
            }
        },
        Cmd::Dataset { action } => match action {
            DatasetCmd::FromSpans {
                data_dir,
                trace_id,
                model,
                limit,
                out,
            } => {
                evald::init_tracing();
                let store = evald::Store::open(
                    &data_dir,
                    StoreConfig {
                        compact_interval: None,
                        ..StoreConfig::default()
                    },
                )
                .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
                let (written, skipped) = evald::dataset::from_spans(
                    &store,
                    &evald::dataset::FromSpansFilter {
                        trace_id,
                        model,
                        limit,
                    },
                    out.as_deref(),
                )?;
                let tail = if skipped > 0 {
                    format!("; skipped {skipped} span(s) with no captured output")
                } else {
                    String::new()
                };
                eprintln!("evald dataset: wrote {written} row(s){tail}");
            }
        },
        Cmd::Query {
            sql,
            data_dir,
            limit,
        } => {
            evald::init_tracing();
            // One-shot: open the store embedded (no background compactor) and query. The
            // SQL layer unions the hot tier with the cold blocks, so un-compacted spans are
            // included without a flush.
            let store = evald::Store::open(
                &data_dir,
                StoreConfig {
                    compact_interval: None,
                    ..StoreConfig::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
            let result = evald::sql::query(&store, &sql, limit).await?;
            // Print as pretty JSON rows (stdout) so it pipes into jq / a CI step.
            println!("{}", serde_json::to_string_pretty(&result.rows)?);
            if result.truncated {
                eprintln!(
                    "(truncated to {} row(s); pass --limit to raise the cap)",
                    result.row_count
                );
            }
        }
        Cmd::Retention {
            older_than,
            data_dir,
            dry_run,
        } => {
            evald::init_tracing();
            // Same helper the background sweep uses, so a one-shot `evald retention` and
            // `serve --retention` can never compute different cutoffs from the same window.
            let cutoff = evald::store::retention_cutoff(older_than)
                .ok_or_else(|| anyhow::anyhow!("system clock is before the unix epoch"))?;
            // One-shot: open the store embedded with no background compactor, sweep, report.
            let store = evald::Store::open(
                &data_dir,
                StoreConfig {
                    compact_interval: None,
                    ..StoreConfig::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
            let report = store
                .reclaim_before(cutoff, dry_run)
                .map_err(|e| anyhow::anyhow!("retention sweep failed: {e}"))?;
            let mib = report.bytes_reclaimed as f64 / (1024.0 * 1024.0);
            let verb = if dry_run { "would drop" } else { "dropped" };
            let oldest = report
                .oldest_kept_unix_nano
                .map(|t| format!("; oldest retained span-start {t} ns"))
                .unwrap_or_default();
            println!(
                "evald retention ({}): {verb} {} block(s), {:.2} MiB reclaimed; {} block(s) kept{}.",
                if dry_run { "dry-run" } else { "applied" },
                report.blocks_dropped,
                mib,
                report.blocks_kept,
                oldest,
            );
        }
        Cmd::Compact {
            data_dir,
            dry_run,
            max_spans,
            days,
        } => {
            evald::init_tracing();
            // One-shot: open the store embedded with no background compactor, merge, report.
            // `MergeScope::Full` ignores the serve-time hour threshold and the closed-day
            // quiet window — an operator running this asked for the merge now.
            let store = evald::Store::open(
                &data_dir,
                StoreConfig {
                    compact_interval: None,
                    merge: evald::store::MergePolicy {
                        max_spans_per_block: max_spans,
                        day_merge: days,
                        ..evald::store::MergePolicy::default()
                    },
                    ..StoreConfig::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
            let before = store.cold_block_count()?;
            let report = store
                .merge_now(evald::store::MergeScope::Full, dry_run)
                .await
                .map_err(|e| anyhow::anyhow!("cold merge failed: {e}"))?;
            // Reclaim the inputs this run retired. They left the index in the merge's own
            // transaction, so this only unlinks files nothing references; the grace is zero
            // because a one-shot run holds the data-dir exclusively — no other process can
            // be part-way through a scan of them.
            let swept = if dry_run {
                0
            } else {
                store
                    .sweep_unindexed(Duration::ZERO)
                    .await
                    .map_err(|e| anyhow::anyhow!("reclaiming merged inputs failed: {e}"))?
            };
            let after = store.cold_block_count()?;
            let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
            if dry_run {
                println!(
                    "evald compact (dry-run): would merge {} block(s) into {} ({} span(s), \
                     {:.2} MiB read); {before} block(s) on disk, unchanged.",
                    report.blocks_in,
                    report.blocks_out,
                    report.spans,
                    mib(report.bytes_in),
                );
                if days && report.merges > 0 {
                    // A preview cannot see the day phase consume blocks the hour phase has
                    // not actually written yet, so the real run collapses at least this far.
                    println!(
                        "  (a real run then collapses the merged hour blocks into day \
                         blocks, so the final count is lower still)"
                    );
                }
            } else {
                println!(
                    "evald compact: merged {} block(s) into {} ({} span(s), {:.2} MiB -> \
                     {:.2} MiB); blocks {before} -> {after}; {swept} input file(s) reclaimed.",
                    report.blocks_in,
                    report.blocks_out,
                    report.spans,
                    mib(report.bytes_in),
                    mib(report.bytes_out),
                );
            }
        }
        Cmd::Migrate { data_dir, dry_run } => {
            evald::init_tracing();
            use evald::store::format::{self, FormatState};
            match format::inspect(&data_dir)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", data_dir.display()))?
            {
                FormatState::Marked(marker) => {
                    // A newer format is an error, not a migration: this build cannot read it.
                    format::check_readable(&marker, &data_dir)?;
                    println!(
                        "evald migrate: {} is already at format {} (written by evald {}); nothing to do.",
                        data_dir.display(),
                        marker.format,
                        marker.evald_version,
                    );
                }
                FormatState::Legacy if dry_run => {
                    println!(
                        "evald migrate (dry-run): {} predates the format marker; \
                         it WOULD be stamped as format {}. No data is rewritten.",
                        data_dir.display(),
                        format::FORMAT_VERSION,
                    );
                }
                FormatState::Legacy => {
                    let marker = format::stamp(&data_dir, Some("legacy"))?;
                    println!(
                        "evald migrate: stamped {} as format {}. No data was rewritten — \
                         the layout was already format {}.",
                        data_dir.display(),
                        marker.format,
                        marker.format,
                    );
                }
                FormatState::Fresh if dry_run => {
                    println!(
                        "evald migrate (dry-run): {} is empty; it WOULD be stamped as format {} \
                         on first use.",
                        data_dir.display(),
                        format::FORMAT_VERSION,
                    );
                }
                FormatState::Fresh => {
                    let marker = format::stamp(&data_dir, None)?;
                    println!(
                        "evald migrate: {} was empty; stamped as format {}.",
                        data_dir.display(),
                        marker.format,
                    );
                }
            }
        }
        Cmd::Version => {
            println!("evald {}", evald::Version::current());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_retention_units() {
        assert_eq!(
            parse_retention("30d").unwrap(),
            Duration::from_secs(30 * 86_400)
        );
        assert_eq!(
            parse_retention("72h").unwrap(),
            Duration::from_secs(72 * 3_600)
        );
        assert_eq!(
            parse_retention("90m").unwrap(),
            Duration::from_secs(90 * 60)
        );
        assert_eq!(
            parse_retention("3600s").unwrap(),
            Duration::from_secs(3_600)
        );
        assert_eq!(parse_retention("45").unwrap(), Duration::from_secs(45)); // bare = seconds
        assert!(parse_retention("").is_err());
        assert!(parse_retention("banana").is_err());
        assert!(parse_retention("12x").is_err());
    }
}
