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

use clap::{Parser, Subcommand};
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
        /// Durable-backlog bound: shed ingest (429 + Retry-After) once this many un-compacted
        /// spans are resident in memory. Everything accepted is already durable in the WAL, so
        /// this bounds memory, not durability — it keeps a lagging compactor from OOMing the
        /// process. `0` disables the bound (unbounded hot tier).
        #[arg(long, default_value_t = 1_000_000, env = "EVALD_MAX_HOT_SPANS")]
        max_hot_spans: usize,
        /// Offload a span input/output payload larger than this many bytes to the blob store,
        /// leaving a compact `evald-blob:<key>` reference on the span (fetch via
        /// `GET /v1/blobs/{key}`). Keeps a megabyte RAG context or tool output out of the WAL /
        /// Parquet / query response. `0` disables offloading (payloads stay inline).
        #[arg(long, default_value_t = 256 * 1024, env = "EVALD_BLOB_OFFLOAD_BYTES")]
        blob_offload_bytes: usize,
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
            max_hot_spans,
            blob_offload_bytes,
        } => {
            evald::init_tracing();
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
            let config = StoreConfig {
                seal_threshold_spans: seal_threshold,
                compact_interval: (compact_interval_secs > 0)
                    .then(|| Duration::from_secs(compact_interval_secs)),
                max_hot_spans,
                blob_offload_bytes,
                ..StoreConfig::default()
            };
            evald::ingest::serve(addr, grpc_addr, data_dir, config).await?;
        }
        Cmd::Eval { action } => match action {
            EvalCmd::Run {
                config,
                data_dir,
                estimate,
            } => {
                evald::init_tracing();
                if estimate {
                    evald::eval::estimate_command(&config, &data_dir)?;
                } else {
                    let passed = evald::eval::run_command(&config, &data_dir).await?;
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
            } => {
                evald::init_tracing();
                let passed = evald::eval::compare_command(
                    &run_a,
                    &run_b,
                    &data_dir,
                    fail_on_regression,
                    tolerance,
                    significance,
                    alpha,
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
        } => {
            evald::init_tracing();
            let dimension = evald::cost::Dimension::parse(&by)?;
            evald::cost::cost_command(dimension, &data_dir, limit).await?;
        }
        Cmd::Suite { action } => match action {
            SuiteCmd::Run { config } => {
                evald::init_tracing();
                let outcome = evald::suite::run_suite(&config)?;
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
            let now_nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| anyhow::anyhow!("system clock is before the unix epoch: {e}"))?
                .as_nanos();
            let cutoff = now_nanos
                .saturating_sub(older_than.as_nanos())
                .min(u64::MAX as u128) as u64;
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
