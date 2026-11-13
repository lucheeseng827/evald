//! Latency percentiles and time-to-first-token (`evald latency`).
//!
//! Exact **nearest-rank** percentiles of `end - start` over LLM spans, grouped by model,
//! provider or service, straight over the `spans` SQL table — no new storage. Nearest-rank
//! means the p-th percentile of `n` sorted values is the value at rank `ceil(p * n)` (1-based):
//! it is always a value that was actually observed, never an interpolation, so `p99` of 50
//! spans is the slowest span, not a number nobody saw.
//!
//! An LLM span is the one [`NormalizedSpan::is_llm_span`] describes and the metrics use, so
//! the CLI, the SQL recipe and `/metrics` agree about which spans count. Retriever, tool and
//! chain spans would otherwise drag every percentile toward "fast".
//!
//! ## Time to first token
//!
//! Read from span attributes **only** and reported as `unknown` when a span carries none;
//! never estimated. [`TTFT_ATTRS`] lists the attribute names read, each checked against the
//! emitting project's source. Nothing new is stored (`raw_attributes` already holds them).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::{Store, StoreConfig};

/// Span attributes that carry a time-to-first-token, in precedence order, with the factor that
/// converts the value to seconds. A span carrying several uses the first listed.
///
/// * `gen_ai.response.time_to_first_chunk` — OpenTelemetry GenAI conventions (seconds, streaming
///   requests); emitted by LiteLLM's OTel integration.
/// * `ai.response.msToFirstChunk`, `ai.stream.msToFirstChunk` — Vercel AI SDK telemetry
///   (milliseconds; the second is the pre-4.0 spelling).
/// * `time_to_first_token_ms` — OpenInference's OpenAI Agents realtime instrumentation
///   (milliseconds; measured from the end of the user's audio, so it is a *response* latency for
///   a voice turn rather than from request issuance).
///
/// * `gen_ai.latency.time_to_first_token` — vLLM's own request span (seconds, measured from
///   the request's arrival at the server; [`crate::vllm`]).
///
/// Projects that emit it only as a metric (OpenLLMetry) leave nothing on the span to read.
pub const TTFT_ATTRS: [(&str, f64); 5] = [
    ("gen_ai.response.time_to_first_chunk", 1.0),
    ("ai.response.msToFirstChunk", 1e-3),
    ("ai.stream.msToFirstChunk", 1e-3),
    ("time_to_first_token_ms", 1e-3),
    ("gen_ai.latency.time_to_first_token", 1.0),
];

/// The time to first token of a span in seconds, if it carries a usable one. A non-numeric,
/// negative or non-finite value counts as absent.
pub fn ttft_seconds(attrs: &BTreeMap<String, serde_json::Value>) -> Option<f64> {
    // One pass over the attributes rather than a lookup per name. This is called for every LLM
    // span the writer commits, and the common span carries none of these names: five
    // `BTreeMap::get`s then cost five tree descents, each comparing whole strings, where one pass
    // compares each attribute against five names and an unequal length settles almost all of it.
    // The name earliest in `TTFT_ATTRS` still wins, so a span carrying two is read the same way.
    let mut best: Option<(usize, f64)> = None;
    for (key, raw) in attrs {
        let Some(i) = TTFT_ATTRS.iter().position(|(name, _)| *name == key) else {
            continue;
        };
        if best.is_some_and(|(seen, _)| seen <= i) {
            continue;
        }
        let Some(v) = raw
            .as_f64()
            .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        else {
            continue;
        };
        if v.is_finite() && v >= 0.0 {
            best = Some((i, v * TTFT_ATTRS[i].1));
        }
    }
    best.map(|(_, v)| v)
}

/// SQL predicate for "this span is an LLM span"; mirrors [`crate::NormalizedSpan::is_llm_span`]
/// (a test holds the two together). `strpos` rather than `LIKE`, because `_` is a `LIKE`
/// wildcard and these keys are full of them.
pub(crate) const LLM_PREDICATE: &str = "(upper(oi_kind) = 'LLM' \
     OR ((oi_kind IS NULL OR upper(oi_kind) NOT IN ('AGENT', 'CHAIN', 'TOOL')) \
         AND (model IS NOT NULL \
              OR strpos(raw_attributes_json, '\"gen_ai.request.') > 0 \
              OR strpos(raw_attributes_json, '\"gen_ai.response.') > 0 \
              OR strpos(raw_attributes_json, '\"gen_ai.usage.') > 0)))";

/// Most spans carrying one TTFT attribute the report will hold in memory (each row is a few
/// short strings). Past it the report errors instead of silently computing percentiles over
/// a truncated set.
const TTFT_ROW_CAP: usize = 10_000_000;

/// The grouping dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Model,
    Provider,
    Service,
}

impl Dimension {
    pub fn parse(s: &str) -> anyhow::Result<Dimension> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "model" => Dimension::Model,
            "provider" => Dimension::Provider,
            "service" => Dimension::Service,
            other => {
                anyhow::bail!("unknown --by {other:?} (expected one of: model, provider, service)")
            }
        })
    }

    fn column(self) -> &'static str {
        match self {
            Dimension::Model => "model",
            Dimension::Provider => "provider",
            Dimension::Service => "service_name",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Dimension::Model => "model",
            Dimension::Provider => "provider",
            Dimension::Service => "service",
        }
    }
}

/// One group's row of the report. Durations are milliseconds.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LatencyRow {
    pub attribution: String,
    pub spans: u64,
    pub errors: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Spans in the group that carry a time to first token.
    pub ttft_known: u64,
    /// `spans - ttft_known`: shown, never estimated.
    pub ttft_unknown: u64,
    pub ttft_p50_ms: Option<f64>,
    pub ttft_p95_ms: Option<f64>,
}

/// The exact-percentile query, verbatim what the docs give as a recipe (with the group column
/// substituted). One window pass ranks each span within its group; the p-th percentile is the
/// smallest duration whose rank is at least `ceil(p * n)`.
fn percentile_sql(dimension: Dimension) -> String {
    let col = dimension.column();
    format!(
        "WITH d AS (SELECT {col} AS g, \
                CASE WHEN end_unix_nano >= start_unix_nano \
                     THEN end_unix_nano - start_unix_nano ELSE 0 END AS dur, \
                status_code \
              FROM spans WHERE {LLM_PREDICATE}), \
         r AS (SELECT g, dur, status_code, \
                ROW_NUMBER() OVER (PARTITION BY g ORDER BY dur) AS rn, \
                COUNT(*) OVER (PARTITION BY g) AS n FROM d) \
         SELECT CASE WHEN g IS NULL THEN '(untagged)' ELSE g END AS attribution, \
                MAX(n) AS spans, \
                SUM(CASE WHEN status_code = 2 THEN 1 ELSE 0 END) AS errors, \
                MIN(CASE WHEN rn >= CEIL(0.50 * n) THEN dur END) AS p50_ns, \
                MIN(CASE WHEN rn >= CEIL(0.95 * n) THEN dur END) AS p95_ns, \
                MIN(CASE WHEN rn >= CEIL(0.99 * n) THEN dur END) AS p99_ns, \
                MAX(dur) AS max_ns \
         FROM r GROUP BY g ORDER BY spans DESC, attribution"
    )
}

/// The recipe shown in `docs/OPERATIONS.md`: per-model exact percentiles. Kept as a constant so
/// a test runs precisely the text the docs quote.
pub const RECIPE_SQL: &str = "WITH d AS (SELECT model AS g, \
end_unix_nano - start_unix_nano AS dur FROM spans WHERE model IS NOT NULL), \
r AS (SELECT g, dur, ROW_NUMBER() OVER (PARTITION BY g ORDER BY dur) AS rn, \
COUNT(*) OVER (PARTITION BY g) AS n FROM d) \
SELECT g AS model, MAX(n) AS spans, \
MIN(CASE WHEN rn >= CEIL(0.50 * n) THEN dur END) / 1e6 AS p50_ms, \
MIN(CASE WHEN rn >= CEIL(0.95 * n) THEN dur END) / 1e6 AS p95_ms, \
MIN(CASE WHEN rn >= CEIL(0.99 * n) THEN dur END) / 1e6 AS p99_ms \
FROM r GROUP BY g ORDER BY spans DESC";

/// Nearest-rank percentile of an ascending slice: the value at rank `ceil(p * n)`.
pub fn nearest_rank(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted.get(rank - 1).copied()
}

fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Compute the report rows. Factored out of [`latency_command`] so it is testable without
/// capturing stdout.
pub async fn latency_rows(
    store: &Store,
    dimension: Dimension,
    limit: usize,
) -> anyhow::Result<Vec<LatencyRow>> {
    let base = crate::sql::query(store, &percentile_sql(dimension), limit).await?;
    let mut rows: Vec<LatencyRow> = base
        .rows
        .iter()
        .map(|r| {
            let ms = |k: &str| num(&r[k]).unwrap_or(0.0) / 1e6;
            LatencyRow {
                attribution: r["attribution"].as_str().unwrap_or("?").to_string(),
                spans: num(&r["spans"]).unwrap_or(0.0) as u64,
                errors: num(&r["errors"]).unwrap_or(0.0) as u64,
                p50_ms: ms("p50_ns"),
                p95_ms: ms("p95_ns"),
                p99_ms: ms("p99_ns"),
                max_ms: ms("max_ns"),
                ttft_known: 0,
                ttft_unknown: 0,
                ttft_p50_ms: None,
                ttft_p95_ms: None,
            }
        })
        .collect();

    // Time to first token: one small query per attribute name, extracting just the number from
    // the stored attribute JSON so a span's prompt text never crosses into this process. A span
    // that carries several names is counted once, under the first in `TTFT_ATTRS`.
    let col = dimension.column();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut ttft: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for (key, to_seconds) in TTFT_ATTRS {
        let re = key.replace('.', "\\.");
        // The same shape the extraction needs. `regexp_replace` returns its INPUT unchanged when
        // the pattern does not match, so without this a span whose TTFT attribute holds a string
        // (`"soon"`), a null or an object would put the whole `raw_attributes_json` — prompt and
        // completion text included — into `v`, up to `TTFT_ROW_CAP` rows of it.
        let number = "\\s*:\\s*\"?-?[0-9]+(?:\\.[0-9]+)?(?:[eE][+-]?[0-9]+)?\"?";
        let sql = format!(
            "SELECT CASE WHEN {col} IS NULL THEN '(untagged)' ELSE {col} END AS attribution, \
                    trace_id, span_id, \
                    regexp_replace(raw_attributes_json, \
                      '(?s)^.*\"{re}\"\\s*:\\s*\"?(-?[0-9]+(?:\\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)\"?.*$', \
                      '$1') AS v \
             FROM spans WHERE {LLM_PREDICATE} AND strpos(raw_attributes_json, '\"{key}\"') > 0 \
               AND regexp_like(raw_attributes_json, '(?s)\"{re}\"{number}')"
        );
        let found = crate::sql::query(store, &sql, TTFT_ROW_CAP).await?;
        anyhow::ensure!(
            !found.truncated,
            "more than {TTFT_ROW_CAP} spans carry `{key}`; the time-to-first-token percentiles \
             would be computed from a truncated set, so none are reported"
        );
        for r in found.rows {
            let id = (
                r["trace_id"].as_str().unwrap_or("").to_string(),
                r["span_id"].as_str().unwrap_or("").to_string(),
            );
            if !seen.insert(id) {
                continue;
            }
            if let Some(v) = num(&r["v"]).filter(|v| v.is_finite() && *v >= 0.0) {
                ttft.entry(r["attribution"].as_str().unwrap_or("?").to_string())
                    .or_default()
                    .push(v * to_seconds);
            }
        }
    }
    for row in &mut rows {
        let mut vals = ttft.remove(&row.attribution).unwrap_or_default();
        vals.sort_by(|a, b| a.total_cmp(b));
        row.ttft_known = vals.len() as u64;
        row.ttft_unknown = row.spans.saturating_sub(row.ttft_known);
        row.ttft_p50_ms = nearest_rank(&vals, 0.50).map(|s| s * 1e3);
        row.ttft_p95_ms = nearest_rank(&vals, 0.95).map(|s| s * 1e3);
    }
    Ok(rows)
}

/// Where an inference server says a model's calls spent their time ([`crate::vllm`]): p50 and
/// p95 (ms) of each phase, and of the client's own duration minus the server's (network,
/// proxies, the client library) for the calls whose client span is the server span's parent.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerRow {
    /// The client span's model, else the server span's, else `(unknown)`.
    pub model: String,
    pub calls: u64,
    /// Calls whose client span is in the store (the server span's parent).
    pub parented: u64,
    /// `(phase, p50_ms, p95_ms)` in [`crate::vllm::PHASES`] order; `None` when no call carried it.
    pub phases: Vec<(&'static str, Option<f64>, Option<f64>)>,
    pub outside_p50_ms: Option<f64>,
    pub outside_p95_ms: Option<f64>,
}

/// Most vLLM request spans the breakdown reads (only numbers cross into this process).
const SERVER_ROW_CAP: usize = 10_000_000;

/// The server-side breakdown over vLLM's request spans, by model. Empty when there are none.
pub async fn server_rows(store: &Store) -> anyhow::Result<Vec<ServerRow>> {
    use crate::vllm::{PHASES, SERVER_KEY, VLLM};
    let number = "-?[0-9]+(?:\\.[0-9]+)?(?:[eE][+-]?[0-9]+)?";
    // One column per phase: the number only, or NULL when the attribute is absent or not a
    // number (`regexp_replace` would otherwise hand back the whole attribute JSON).
    let cols: Vec<String> = PHASES
        .iter()
        .map(|(name, key)| {
            let re = key.replace('.', "\\.");
            format!(
                "CASE WHEN regexp_like(s.raw_attributes_json, '(?s)\"{re}\"\\s*:\\s*{number}') \
                 THEN regexp_replace(s.raw_attributes_json, \
                   '(?s)^.*\"{re}\"\\s*:\\s*({number}).*$', '$1') END AS {name}"
            )
        })
        .collect();
    let sql = format!(
        "SELECT COALESCE(c.model, s.model) AS model, \
                c.end_unix_nano - c.start_unix_nano AS client_ns, {} \
         FROM spans s LEFT JOIN spans c \
           ON c.trace_id = s.trace_id AND c.span_id = s.parent_span_id \
         WHERE strpos(s.raw_attributes_json, '\"{SERVER_KEY}\":\"{VLLM}\"') > 0",
        cols.join(", ")
    );
    let found = crate::sql::query(store, &sql, SERVER_ROW_CAP).await?;
    anyhow::ensure!(
        !found.truncated,
        "more than {SERVER_ROW_CAP} vLLM request spans; the breakdown would be computed from a \
         truncated set, so none is reported"
    );
    #[derive(Default)]
    struct Acc {
        calls: u64,
        parented: u64,
        phases: Vec<Vec<f64>>,
        outside: Vec<f64>,
    }
    let mut by_model: BTreeMap<String, Acc> = BTreeMap::new();
    for r in &found.rows {
        let model = r["model"].as_str().unwrap_or("(unknown)").to_string();
        let acc = by_model.entry(model).or_insert_with(|| Acc {
            phases: vec![Vec::new(); PHASES.len()],
            ..Acc::default()
        });
        acc.calls += 1;
        let secs: Vec<Option<f64>> = PHASES
            .iter()
            .map(|(name, _)| num(&r[*name]).filter(|v| v.is_finite() && *v >= 0.0))
            .collect();
        for (i, v) in secs.iter().enumerate() {
            if let Some(v) = v {
                acc.phases[i].push(v * 1e3);
            }
        }
        if let Some(client_ns) = num(&r["client_ns"]) {
            acc.parented += 1;
            if let Some(server_s) = secs[PHASES.len() - 1] {
                acc.outside
                    .push((client_ns / 1e6 - server_s * 1e3).max(0.0));
            }
        }
    }
    Ok(by_model
        .into_iter()
        .map(|(model, mut a)| {
            for v in a.phases.iter_mut().chain([&mut a.outside]) {
                v.sort_by(f64::total_cmp);
            }
            ServerRow {
                model,
                calls: a.calls,
                parented: a.parented,
                phases: PHASES
                    .iter()
                    .zip(&a.phases)
                    .map(|((name, _), v)| (*name, nearest_rank(v, 0.50), nearest_rank(v, 0.95)))
                    .collect(),
                outside_p50_ms: nearest_rank(&a.outside, 0.50),
                outside_p95_ms: nearest_rank(&a.outside, 0.95),
            }
        })
        .collect())
}

fn print_server(rows: &[ServerRow]) {
    if rows.is_empty() {
        return;
    }
    println!(
        "
server time — vLLM request spans, by model (p50 / p95, ms)"
    );
    let pair = |p50: Option<f64>, p95: Option<f64>| format!("{} / {}", ms_cell(p50), ms_cell(p95));
    print!("{:<28} {:>7} {:>8}", "model", "calls", "parented");
    for (name, _) in crate::vllm::PHASES {
        print!(" {name:>17}");
    }
    println!(" {:>17}", "client - server");
    for r in rows {
        print!("{:<28} {:>7} {:>8}", r.model, r.calls, r.parented);
        for (_, p50, p95) in &r.phases {
            print!(" {:>17}", pair(*p50, *p95));
        }
        println!(" {:>17}", pair(r.outside_p50_ms, r.outside_p95_ms));
    }
    println!(
        "
note: `client - server` is the app's own span of the call minus vLLM's end-to-end time \
         (network, proxies, the client library), over the calls whose app span is the parent."
    );
}

/// `evald latency` — open the store, compute the report, print it.
pub async fn latency_command(
    dimension: Dimension,
    data_dir: &Path,
    limit: usize,
) -> anyhow::Result<()> {
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let rows = latency_rows(&store, dimension, limit).await?;
    print_report(dimension, &rows);
    print_server(&server_rows(&store).await?);
    Ok(())
}

fn ms_cell(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"))
}

fn print_report(dimension: Dimension, rows: &[LatencyRow]) {
    println!(
        "\nevald latency — LLM spans by {} (nearest-rank, ms)",
        dimension.label()
    );
    println!(
        "{:<28} {:>8} {:>6} {:>10} {:>10} {:>10} {:>10} {:>8} {:>8} {:>10} {:>10}",
        dimension.label(),
        "spans",
        "errors",
        "p50",
        "p95",
        "p99",
        "max",
        "ttft_n",
        "unknown",
        "ttft_p50",
        "ttft_p95"
    );
    for r in rows {
        let key: String = if r.attribution.chars().count() <= 28 {
            r.attribution.clone()
        } else {
            let mut t: String = r.attribution.chars().take(27).collect();
            t.push('…');
            t
        };
        println!(
            "{:<28} {:>8} {:>6} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>8} {:>8} {:>10} {:>10}",
            key,
            r.spans,
            r.errors,
            r.p50_ms,
            r.p95_ms,
            r.p99_ms,
            r.max_ms,
            r.ttft_known,
            r.ttft_unknown,
            ms_cell(r.ttft_p50_ms),
            ms_cell(r.ttft_p95_ms)
        );
    }
    let unknown: u64 = rows.iter().map(|r| r.ttft_unknown).sum();
    if unknown > 0 {
        println!(
            "\nnote: `unknown` counts spans with no time-to-first-token attribute; it is never \
             estimated. Read from: {}.",
            TTFT_ATTRS
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_span;
    use serde_json::json;

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        (dir, store)
    }

    fn llm(id: &str, model: Option<&str>, dur_ms: u64) -> crate::NormalizedSpan {
        let mut s = test_span("t", id, 1_000_000_000);
        s.oi_kind = Some("LLM".into());
        s.model = model.map(String::from);
        s.end_unix_nano = s.start_unix_nano + dur_ms * 1_000_000;
        s
    }

    #[test]
    fn nearest_rank_is_an_observed_value() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(nearest_rank(&v, 0.50), Some(50.0));
        assert_eq!(nearest_rank(&v, 0.95), Some(95.0));
        assert_eq!(nearest_rank(&v, 0.99), Some(99.0));
        assert_eq!(
            nearest_rank(&[7.0], 0.99),
            Some(7.0),
            "n=1: every percentile is the value"
        );
        assert_eq!(nearest_rank(&[1.0, 2.0], 0.50), Some(1.0), "ceil(0.5*2)=1");
        assert_eq!(nearest_rank(&[1.0, 2.0], 0.51), Some(2.0), "ceil(0.51*2)=2");
        assert_eq!(nearest_rank(&[], 0.5), None);
    }

    #[test]
    fn ttft_reads_each_documented_attribute_in_seconds_and_rejects_junk() {
        let a = |k: &str, v: serde_json::Value| {
            let mut m = BTreeMap::new();
            m.insert(k.to_string(), v);
            m
        };
        assert_eq!(
            ttft_seconds(&a("gen_ai.response.time_to_first_chunk", json!(0.5))),
            Some(0.5)
        );
        assert_eq!(
            ttft_seconds(&a("ai.response.msToFirstChunk", json!(250))),
            Some(0.25)
        );
        assert_eq!(
            ttft_seconds(&a("ai.stream.msToFirstChunk", json!(100))),
            Some(0.1)
        );
        assert_eq!(
            ttft_seconds(&a("time_to_first_token_ms", json!("40"))),
            Some(0.04)
        );
        assert_eq!(
            ttft_seconds(&a("gen_ai.response.time_to_first_chunk", json!(-1))),
            None
        );
        assert_eq!(
            ttft_seconds(&a("gen_ai.response.time_to_first_chunk", json!("soon"))),
            None
        );
        assert_eq!(ttft_seconds(&a("gen_ai.response.model", json!(1))), None);
        // First listed wins when a span carries two.
        let mut both = a("gen_ai.response.time_to_first_chunk", json!(2.0));
        both.insert("ai.response.msToFirstChunk".into(), json!(9));
        assert_eq!(ttft_seconds(&both), Some(2.0));
    }

    #[tokio::test]
    async fn exact_percentiles_per_model_over_llm_spans_only() {
        let (_d, store) = open();
        let mut spans: Vec<_> = (1..=100)
            .map(|i| llm(&format!("{i:03}"), Some("gpt-4o"), i))
            .collect();
        spans.push(llm("e1", Some("claude"), 40));
        spans.push(llm("e2", Some("claude"), 10));
        // Not LLM spans: a retriever with a huge duration must not move any percentile.
        let mut retriever = test_span("t", "r1", 1_000_000_000);
        retriever.oi_kind = Some("RETRIEVER".into());
        retriever.end_unix_nano = retriever.start_unix_nano + 999_000_000_000;
        spans.push(retriever);
        store.append(spans).await.unwrap();

        let rows = latency_rows(&store, Dimension::Model, 100).await.unwrap();
        assert_eq!(
            rows.len(),
            2,
            "the retriever's null model is not a group: {rows:?}"
        );
        let g = rows.iter().find(|r| r.attribution == "gpt-4o").unwrap();
        assert_eq!(g.spans, 100);
        assert_eq!(
            (g.p50_ms, g.p95_ms, g.p99_ms, g.max_ms),
            (50.0, 95.0, 99.0, 100.0)
        );
        let c = rows.iter().find(|r| r.attribution == "claude").unwrap();
        assert_eq!(c.spans, 2);
        assert_eq!((c.p50_ms, c.p95_ms, c.max_ms), (10.0, 40.0, 40.0));
    }

    #[tokio::test]
    async fn the_sql_llm_filter_agrees_with_is_llm_span() {
        let (_d, store) = open();
        let mut v = Vec::new();
        let mut a = test_span("t", "a", 1);
        a.oi_kind = Some("llm".into()); // case-insensitive
        v.push(a);
        let mut b = test_span("t", "b", 2);
        b.model = Some("m".into());
        v.push(b);
        let mut c = test_span("t", "c", 3);
        c.raw_attributes
            .insert("gen_ai.usage.input_tokens".into(), json!(5));
        v.push(c);
        let mut d = test_span("t", "d", 4);
        d.raw_attributes
            .insert("gen_ai.tool.name".into(), json!("x")); // a tool span: not LLM
        v.push(d);
        let mut e = test_span("t", "e", 5);
        e.oi_kind = Some("TOOL".into());
        v.push(e);
        let expected = v.iter().filter(|s| s.is_llm_span()).count() as u64;
        assert_eq!(expected, 3);
        store.append(v).await.unwrap();

        let rows = latency_rows(&store, Dimension::Service, 100).await.unwrap();
        assert_eq!(rows.iter().map(|r| r.spans).sum::<u64>(), expected);
        assert_eq!(rows[0].attribution, "(untagged)");
    }

    #[tokio::test]
    async fn ttft_known_and_unknown_are_counted_and_never_estimated() {
        let (_d, store) = open();
        let mut s1 = llm("1", Some("m"), 900);
        s1.raw_attributes
            .insert("gen_ai.response.time_to_first_chunk".into(), json!(0.2));
        let mut s2 = llm("2", Some("m"), 900);
        s2.raw_attributes
            .insert("ai.response.msToFirstChunk".into(), json!(400));
        // Carries two names: counted once, under the first listed (0.6 s, not 8 s).
        let mut s3 = llm("3", Some("m"), 900);
        s3.raw_attributes
            .insert("gen_ai.response.time_to_first_chunk".into(), json!(0.6));
        s3.raw_attributes
            .insert("ai.response.msToFirstChunk".into(), json!(8000));
        let s4 = llm("4", Some("m"), 900); // no TTFT
        store.append(vec![s1, s2, s3, s4]).await.unwrap();

        let rows = latency_rows(&store, Dimension::Model, 100).await.unwrap();
        let r = &rows[0];
        assert_eq!((r.spans, r.ttft_known, r.ttft_unknown), (4, 3, 1));
        // sorted known: 0.2, 0.4, 0.6 s -> p50 = rank ceil(1.5)=2 -> 400 ms; p95 = rank 3 -> 600 ms
        assert!((r.ttft_p50_ms.unwrap() - 400.0).abs() < 1e-6, "{r:?}");
        assert!((r.ttft_p95_ms.unwrap() - 600.0).abs() < 1e-6, "{r:?}");
    }

    #[tokio::test]
    async fn a_non_numeric_ttft_counts_as_unknown_and_never_reaches_this_process() {
        // `regexp_replace` hands back its input when the pattern misses, so a span whose TTFT
        // attribute is not a number must not be selected at all — otherwise the row's value is
        // the whole attribute JSON, prompt text and all.
        let (_d, store) = open();
        let mut good = llm("1", Some("m"), 900);
        good.raw_attributes
            .insert("gen_ai.response.time_to_first_chunk".into(), json!(0.2));
        let mut junk = llm("2", Some("m"), 900);
        junk.raw_attributes
            .insert("gen_ai.response.time_to_first_chunk".into(), json!("soon"));
        junk.raw_attributes.insert(
            "input.value".into(),
            json!("PROMPT-TEXT-THAT-MUST-NOT-BE-READ"),
        );
        store.append(vec![good, junk]).await.unwrap();

        let rows = latency_rows(&store, Dimension::Model, 100).await.unwrap();
        let r = &rows[0];
        assert_eq!(
            (r.spans, r.ttft_known, r.ttft_unknown),
            (2, 1, 1),
            "a non-numeric TTFT is unknown, never estimated: {r:?}"
        );
        assert!((r.ttft_p50_ms.unwrap() - 200.0).abs() < 1e-6, "{r:?}");
    }

    #[tokio::test]
    async fn the_documented_sql_recipe_runs_and_matches() {
        let (_d, store) = open();
        let spans: Vec<_> = (1..=100)
            .map(|i| llm(&format!("{i:03}"), Some("gpt-4o"), i))
            .collect();
        store.append(spans).await.unwrap();
        let r = crate::sql::query(&store, RECIPE_SQL, 10).await.unwrap();
        assert_eq!(r.rows.len(), 1);
        let row = &r.rows[0];
        assert_eq!(row["model"], "gpt-4o");
        assert_eq!(num(&row["p50_ms"]), Some(50.0));
        assert_eq!(num(&row["p95_ms"]), Some(95.0));
        assert_eq!(num(&row["p99_ms"]), Some(99.0));
    }

    #[test]
    fn dimension_parse_rejects_high_cardinality_dimensions() {
        assert_eq!(Dimension::parse("SERVICE").unwrap(), Dimension::Service);
        assert!(Dimension::parse("user").is_err());
        assert!(Dimension::parse("session").is_err());
    }

    /// An app's model call to vLLM and vLLM's own span of it, with `traceparent` propagated.
    fn call_through_vllm(server_parented: bool) -> Vec<crate::NormalizedSpan> {
        use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
        use opentelemetry_proto::tonic::common::v1::{any_value::Value, AnyValue, KeyValue};
        use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
        let kv = |k: &str, v: Value| KeyValue {
            key: k.into(),
            value: Some(AnyValue { value: Some(v) }),
            ..Default::default()
        };
        let client = Span {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            name: "chat Qwen2.5-7B".into(),
            kind: 3,
            start_time_unix_nano: 1_000_000_000,
            end_time_unix_nano: 2_000_000_000, // 1000 ms, as the app saw it
            attributes: vec![
                kv("gen_ai.operation.name", Value::StringValue("chat".into())),
                kv(
                    "gen_ai.request.model",
                    Value::StringValue("Qwen2.5-7B".into()),
                ),
                kv("gen_ai.usage.input_tokens", Value::IntValue(512)),
                kv("gen_ai.usage.output_tokens", Value::IntValue(64)),
            ],
            ..Default::default()
        };
        let server = Span {
            trace_id: vec![1; 16],
            span_id: vec![3; 8],
            parent_span_id: if server_parented { vec![2; 8] } else { vec![] },
            name: "llm_request".into(),
            kind: 2,
            start_time_unix_nano: 1_050_000_000,
            end_time_unix_nano: 1_950_000_000,
            attributes: vec![
                kv("gen_ai.latency.e2e", Value::DoubleValue(0.9)),
                kv("gen_ai.latency.time_in_queue", Value::DoubleValue(0.12)),
                kv(
                    "gen_ai.latency.time_in_model_prefill",
                    Value::DoubleValue(0.19),
                ),
                kv(
                    "gen_ai.latency.time_in_model_decode",
                    Value::DoubleValue(0.59),
                ),
                kv(
                    "gen_ai.latency.time_to_first_token",
                    Value::DoubleValue(0.31),
                ),
                kv("gen_ai.usage.prompt_tokens", Value::IntValue(512)),
                kv("gen_ai.usage.completion_tokens", Value::IntValue(64)),
            ],
            ..Default::default()
        };
        let scope = |spans| ResourceSpans {
            scope_spans: vec![ScopeSpans {
                spans,
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::normalize::normalize_request(&ExportTraceServiceRequest {
            resource_spans: vec![scope(vec![client]), scope(vec![server])],
        })
    }

    #[tokio::test]
    async fn a_call_traced_at_both_ends_counts_once_and_shows_where_the_time_went() {
        let (_d, store) = open();
        store
            .ingest_batch(call_through_vllm(true), &[])
            .await
            .unwrap();

        // One model call, the app's: its tokens once, not twice.
        let cost = crate::cost::cost_rows(&store, crate::cost::Dimension::Model, 10)
            .await
            .unwrap();
        let tokens: i64 = cost.iter().map(|r| r["tokens"].as_i64().unwrap_or(0)).sum();
        assert_eq!(tokens, 576, "{cost:#?}");
        let lat = latency_rows(&store, Dimension::Model, 10).await.unwrap();
        assert_eq!(lat.len(), 1);
        assert_eq!(lat[0].attribution, "Qwen2.5-7B");
        assert_eq!(lat[0].spans, 1);

        // The server's view, under the app's model, with what the server did not account for.
        let rows = server_rows(&store).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:#?}");
        let r = &rows[0];
        assert_eq!(
            (r.model.as_str(), r.calls, r.parented),
            ("Qwen2.5-7B", 1, 1)
        );
        let phase = |n: &str| r.phases.iter().find(|p| p.0 == n).unwrap().1.unwrap();
        assert!((phase("queue") - 120.0).abs() < 1e-6);
        assert!((phase("prefill") - 190.0).abs() < 1e-6);
        assert!((phase("decode") - 590.0).abs() < 1e-6);
        assert!((phase("server") - 900.0).abs() < 1e-6);
        assert!(
            (r.outside_p50_ms.unwrap() - 100.0).abs() < 1e-6,
            "1000 ms seen, 900 served"
        );
    }

    #[tokio::test]
    async fn an_unparented_server_span_is_the_only_record_of_its_call() {
        let (_d, store) = open();
        let spans: Vec<_> = call_through_vllm(false)
            .into_iter()
            .filter(|s| s.name == "llm_request")
            .collect();
        store.ingest_batch(spans, &[]).await.unwrap();
        let lat = latency_rows(&store, Dimension::Model, 10).await.unwrap();
        assert_eq!(lat[0].spans, 1, "it is a model call");
        assert_eq!(lat[0].ttft_known, 1, "with vLLM's time to first token");
        assert!((lat[0].ttft_p50_ms.unwrap() - 310.0).abs() < 1e-6);
        let rows = server_rows(&store).await.unwrap();
        assert_eq!((rows[0].model.as_str(), rows[0].parented), ("(unknown)", 0));
        assert_eq!(rows[0].outside_p50_ms, None);
        assert!(server_rows(&open().1).await.unwrap().is_empty());
    }
}
