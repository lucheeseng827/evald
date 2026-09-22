//! Prometheus metrics for `evald serve` — the `/metrics` scrape surface.
//!
//! Hand-rolled rather than pulled from a metrics crate, for the same reason the rest of the
//! default build is: the [text exposition format][fmt] is a dozen lines of string building,
//! and `prometheus`/`metrics` would add a dependency tree (and, in some versions, a protobuf
//! path) to the air-gapped core in exchange for nothing this endpoint needs. No registry, no
//! global state — the numbers are read from the live [`Store`] at scrape time.
//!
//! [fmt]: https://prometheus.io/docs/instrumenting/exposition_formats/
//!
//! ## What is deliberately NOT here
//!
//! Every series below is O(1) or bounded-small to produce. The store *can* answer "how many
//! spans are there in total" and "how many scores" — [`Store::span_count`] and
//! [`Store::score_count`] — but both walk the whole store (span_count materializes every
//! span in memory; score_count iterates the entire redb table). On a 15-second scrape that
//! turns the monitoring endpoint into the outage it was installed to warn about. Whole-store
//! inventory belongs in `/v1/sql`, where the caller knowingly pays for the scan.
//!
//! Filesystem free space is likewise absent: it is not something evald knows better than the
//! node exporter already running beside it, and guessing at a mount point would be worse than
//! the honest boundary. `evald_wal_bytes` IS here, because the WAL's size is evald's own
//! business — it is the compactor's backlog made visible.

use crate::Store;
use std::fmt::Write as _;

/// The `Content-Type` a Prometheus scrape expects. Version `0.0.4` is the text exposition
/// format every Prometheus-compatible scraper (Prometheus, VictoriaMetrics, Grafana Agent,
/// OTel Collector's prometheus receiver) parses.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Render the current store state as a Prometheus text-format scrape body.
pub fn render(store: &Store) -> String {
    let stats = store.ingest_stats();
    let mut out = String::with_capacity(2048);

    // `build_info` is the conventional way to expose a version: a constant `1` carrying the
    // detail as labels, so a dashboard can join on it and an upgrade is visible as a series
    // change rather than a value change.
    gauge_labeled(
        &mut out,
        "evald_build_info",
        "Build metadata; always 1. The version is carried in the label.",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );

    counter(
        &mut out,
        "evald_spans_ingested_total",
        "Spans durably ACK'd since process start, counted at the fsync that commits them.",
        stats.spans_ingested as f64,
    );
    counter(
        &mut out,
        "evald_spans_shed_total",
        "Spans shed by durable-backlog backpressure (answered 429) since process start.",
        stats.rejections as f64,
    );
    gauge(
        &mut out,
        "evald_hot_spans",
        "Un-compacted spans resident in the hot tier — the backlog the compactor must drain.",
        stats.hot_spans as f64,
    );
    gauge(
        &mut out,
        "evald_hot_spans_max",
        "Configured hot-tier bound; ingest sheds at or above it. 0 means unbounded.",
        stats.max_hot_spans as f64,
    );
    gauge(
        &mut out,
        "evald_ingest_shedding",
        "1 while ingest is shedding (hot tier at its bound), 0 otherwise.",
        if stats.shedding { 1.0 } else { 0.0 },
    );
    gauge(
        &mut out,
        "evald_ingest_channel_capacity",
        "Ingest channel depth: in-flight append commands before a channel-full shed.",
        stats.channel_capacity as f64,
    );
    counter(
        &mut out,
        "evald_compactions_total",
        "Background compaction passes completed since process start.",
        stats.compactions as f64,
    );
    counter(
        &mut out,
        "evald_compaction_failures_total",
        "Background compaction passes that failed since process start.",
        stats.compaction_failures as f64,
    );
    gauge(
        &mut out,
        "evald_cold_blocks",
        "Committed cold Parquet blocks — the file set a full scan opens. Bounded by cold-to-cold merging; unbounded growth here is what ends in EMFILE on the read path.",
        stats.cold_blocks as f64,
    );
    counter(
        &mut out,
        "evald_cold_merges_total",
        "Cold-to-cold merges completed since process start.",
        stats.cold_merges as f64,
    );
    counter(
        &mut out,
        "evald_cold_blocks_merged_total",
        "Cold blocks consumed by those merges since process start.",
        stats.cold_blocks_merged as f64,
    );
    counter(
        &mut out,
        "evald_cold_merge_failures_total",
        "Cold-to-cold merge passes that failed since process start.",
        stats.cold_merge_failures as f64,
    );
    counter(
        &mut out,
        "evald_spans_disk_blocked_total",
        "Spans refused because the data-dir filesystem was below the free-space floor.",
        stats.disk_blocked_spans as f64,
    );
    gauge(
        &mut out,
        "evald_disk_blocked",
        "1 while ingest is refused because free space is under the floor, 0 otherwise.",
        if stats.disk_blocked { 1.0 } else { 0.0 },
    );
    counter(
        &mut out,
        "evald_retention_sweeps_total",
        "Automatic retention sweeps completed since process start.",
        stats.retention_sweeps as f64,
    );
    counter(
        &mut out,
        "evald_retention_blocks_dropped_total",
        "Cold Parquet blocks dropped by automatic retention since process start.",
        stats.retention_blocks_dropped as f64,
    );
    counter(
        &mut out,
        "evald_retention_bytes_reclaimed_total",
        "Bytes reclaimed by automatic retention since process start.",
        stats.retention_bytes_reclaimed as f64,
    );
    // Emitted only when the guardrail actually sampled a value. A disabled or failing probe
    // must not publish a `0` — that reads as "disk full" on every dashboard and alert built
    // over this series, which is precisely the false page a guardrail should not cause.
    if let Some(free) = stats.disk_free_bytes {
        gauge(
            &mut out,
            "evald_disk_free_bytes",
            "Free bytes on the data-dir filesystem at the guardrail's last sample.",
            free as f64,
        );
    }
    counter_labeled(
        &mut out,
        "evald_redactions_total",
        "Sensitive values rewritten on ingest, by rule. Counts OCCURRENCES in the stored representation, not distinct values: normalize promotes input.value/output.value into their own fields while also preserving them in raw_attributes, so one email in a prompt is rewritten - and counted - in both copies.",
        "rule",
        &store.redaction_counts(),
    );
    let (eval_events, eval_events_bad) = crate::evalevent::counts();
    counter(
        &mut out,
        "evald_eval_events_ingested_total",
        "gen_ai.evaluation.result span events stored as scores.",
        eval_events as f64,
    );
    counter(
        &mut out,
        "evald_eval_events_malformed_total",
        "gen_ai.evaluation.result span events dropped as malformed (no evaluation name, or no value, label or error type). The spans in the same request are unaffected.",
        eval_events_bad as f64,
    );
    gauge(
        &mut out,
        "evald_wal_bytes",
        "Bytes currently held in the write-ahead log (spans not yet compacted to Parquet).",
        store.wal_bytes() as f64,
    );

    // Usage series (cost / tokens / latency / rolling scores): recorded by the writer at the
    // commit point, rendered here. Absent entirely under `--no-usage-metrics`.
    if let Some(usage) = store.usage() {
        usage.render(&mut out);
    }

    out
}

/// The three lines of one unlabeled series: HELP, TYPE, value.
fn series(out: &mut String, name: &str, help: &str, kind: &str, labels: &str, value: f64) {
    let _ = writeln!(out, "# HELP {name} {}", escape_help(help));
    let _ = writeln!(out, "# TYPE {name} {kind}");
    let _ = writeln!(out, "{name}{labels} {}", format_value(value));
}

fn counter(out: &mut String, name: &str, help: &str, value: f64) {
    series(out, name, help, "counter", "", value);
}

fn gauge(out: &mut String, name: &str, help: &str, value: f64) {
    series(out, name, help, "gauge", "", value);
}

/// A counter with one series per label value, e.g. one line per redaction rule. Emits HELP
/// and TYPE once, then a line per series — the shape a scraper expects for a labeled family.
/// Nothing is emitted for an empty set, so a disabled feature publishes no series at all
/// rather than a family of zeros.
fn counter_labeled(
    out: &mut String,
    name: &str,
    help: &str,
    label: &str,
    series: &[(String, u64)],
) {
    if series.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP {name} {}", escape_help(help));
    let _ = writeln!(out, "# TYPE {name} counter");
    for (value, count) in series {
        let _ = writeln!(
            out,
            "{name}{{{label}=\"{}\"}} {}",
            escape_label(value),
            format_value(*count as f64)
        );
    }
}

fn gauge_labeled(out: &mut String, name: &str, help: &str, labels: &[(&str, &str)], value: f64) {
    let rendered = format!(
        "{{{}}}",
        labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
            .collect::<Vec<_>>()
            .join(",")
    );
    series(out, name, help, "gauge", &rendered, value);
}

/// Format a value the way the exposition format wants it: integers without a `.0` tail (so
/// `evald_spans_ingested_total 42`, not `42.0`), everything else as a plain decimal. Non-finite
/// values cannot arise from the counters above, but the format spells them `+Inf`/`-Inf`/`NaN`
/// and a scraper rejects the whole body on a malformed line, so they are handled rather than
/// left to `{}`'s `inf`.
pub(crate) fn format_value(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v.is_sign_positive() { "+Inf" } else { "-Inf" }.to_string();
    }
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// In HELP text, `\` and newlines are escaped; quotes are NOT (unlike label values).
pub(crate) fn escape_help(s: &str) -> String {
    s.replace('\\', r"\\").replace('\n', r"\n")
}

/// In a label value, `\`, `"` and newlines are escaped.
pub(crate) fn escape_label(s: &str) -> String {
    s.replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('\n', r"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_formatting_matches_the_exposition_format() {
        // Integers render without a decimal tail — a counter reading "42.0" is legal but
        // noisy, and every other exporter emits "42".
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(42.0), "42");
        assert_eq!(format_value(1.5), "1.5");
        // Non-finite values use the format's spellings, not Rust's `inf`/`NaN` Display,
        // because a scraper rejects the ENTIRE body on one malformed line.
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_value(f64::NAN), "NaN");
    }

    #[test]
    fn label_and_help_escaping_follow_their_different_rules() {
        // A quote is escaped in a label value...
        assert_eq!(escape_label(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_label(r"a\b"), r"a\\b");
        assert_eq!(escape_label("a\nb"), r"a\nb");
        // ...but NOT in HELP text, where only backslash and newline are special.
        assert_eq!(escape_help(r#"a"b"#), r#"a"b"#);
        assert_eq!(escape_help("a\nb"), r"a\nb");
    }

    #[test]
    fn a_series_renders_help_type_and_value_in_order() {
        let mut out = String::new();
        counter(&mut out, "evald_x_total", "How many x.", 7.0);
        assert_eq!(
            out,
            "# HELP evald_x_total How many x.\n# TYPE evald_x_total counter\nevald_x_total 7\n"
        );
    }

    #[test]
    fn build_info_carries_the_version_as_a_label() {
        let mut out = String::new();
        gauge_labeled(
            &mut out,
            "evald_build_info",
            "Build.",
            &[("version", "9.9.9")],
            1.0,
        );
        assert!(
            out.contains(r#"evald_build_info{version="9.9.9"} 1"#),
            "{out}"
        );
    }
}
