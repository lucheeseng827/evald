//! LLM usage series for `/metrics`: cost, tokens, latency and rolling eval scores.
//!
//! Recorded by the store's writer task for each batch **after** the fsync that commits it and
//! **before** the ACK — the same point `evald_spans_ingested_total` is counted — so the series
//! cover exactly the spans that were accepted. WAL replay at startup goes through
//! `Store::open`, never through the writer, so a restart does not re-count what it recovers.
//!
//! ## Cost of the hot path
//!
//! One `Mutex` acquisition per committed batch (not per span; the only other locker is a
//! scrape every few seconds). Inside it a span costs a handful of integer hash lookups and
//! additions: label values are interned to `u32` ids once, the series key is three of them, and
//! nothing is allocated for a value that is already known.
//!
//! ## Cardinality
//!
//! Labels are `gen_ai_provider_name`, `gen_ai_request_model` and `service_name` only — never
//! user or session. Each is interned with a cap; a value past the cap is folded into `other`
//! and counted in `evald_usage_labels_folded_total`. The number of label tuples is itself
//! capped ([`MAX_SERIES`]). So the series count is bounded whatever the traffic, and the
//! interner never grows past its cap.
//!
//! ## What these numbers are
//!
//! Counters are "since process start" and **approximate under exporter retries**: an OTLP
//! exporter that resends a batch it did not get an ACK for is counted twice, because the store
//! only de-duplicates at read time. `POST /v1/sql` over the stored spans is exact.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::metrics::{escape_help, escape_label, format_value};
use crate::{NormalizedSpan, Score, ScoreTarget};

/// OTLP `Status.code` for an error (`STATUS_CODE_ERROR`).
const STATUS_ERROR: i32 = 2;

/// Histogram bounds, in seconds, for operation duration and time-to-first-chunk. The first 14
/// are the OpenTelemetry GenAI conventions' advisory boundaries (10 ms doubling to 81.92 s), so
/// these histograms merge with ones a client SDK emits; the last two extend to ~5.5 minutes
/// because reasoning models and long generations routinely exceed 82 s and would otherwise all
/// land in `+Inf`, where a p95 has no resolution.
pub const TIME_BUCKETS_S: [f64; 16] = [
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92, 163.84,
    327.68,
];

/// Hard ceiling on distinct `(provider, model, service)` tuples. Past it, a new tuple is
/// folded into `(other, other, other)`.
pub const MAX_SERIES: usize = 2048;
/// Caps for the two labels that are normally tiny. The model cap is configurable
/// ([`UsageConfig::model_cap`]); these are not, because a fleet with more than this many
/// providers or services is better served by a per-service evald than by a bigger label space.
const PROVIDER_CAP: usize = 16;
const SERVICE_CAP: usize = 32;
/// Rolling-score window (most recent scores kept per evaluator) and how many evaluator names
/// are tracked. 64 × 1024 × 8 bytes is 512 KiB worst case.
const SCORE_WINDOW: usize = 1024;
const SCORE_NAMES_CAP: usize = 64;

/// Tunables for [`UsageMetrics`]; carried on `StoreConfig`.
#[derive(Debug, Clone)]
pub struct UsageConfig {
    /// Record the usage series at all (`--no-usage-metrics` turns this off).
    pub enabled: bool,
    /// Distinct model names labelled before the rest fold into `other`. 100 comfortably covers
    /// a fleet's live models (each provider offers a dozen or two) while keeping the series
    /// count, and so the scrape body and the Prometheus memory it costs, small.
    pub model_cap: usize,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model_cap: 100,
        }
    }
}

/// Label value `0` is always `other`.
struct Interner {
    cap: usize,
    ids: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            ids: HashMap::new(),
            names: vec!["other".to_string()],
        }
    }

    /// The id for `value`, or `0` (`other`) when the cap is reached. Allocates only when a
    /// new value is admitted, which happens at most `cap` times in the process's life.
    fn intern(&mut self, value: &str, folded: &mut bool) -> u32 {
        if let Some(&id) = self.ids.get(value) {
            return id;
        }
        if self.names.len() - 1 < self.cap {
            let id = self.names.len() as u32;
            self.names.push(value.to_string());
            self.ids.insert(value.to_string(), id);
            id
        } else {
            *folded = true;
            0
        }
    }
}

/// A cumulative-on-render histogram: `counts[i]` holds observations in `(bounds[i-1], bounds[i]]`;
/// observations above the last bound are only in `count`.
struct Hist {
    bounds: &'static [f64],
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Hist {
    fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: vec![0; bounds.len()],
            sum: 0.0,
            count: 0,
        }
    }

    fn observe(&mut self, v: f64) {
        // First bound >= v, so `le` semantics ("less than or equal") hold at the edge.
        let i = self.bounds.partition_point(|b| *b < v);
        if let Some(c) = self.counts.get_mut(i) {
            *c += 1;
        }
        self.sum += v;
        self.count += 1;
    }

    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let mut cum = 0u64;
        for (i, b) in self.bounds.iter().enumerate() {
            cum += self.counts[i];
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels}{}le=\"{}\"}} {cum}",
                if labels.is_empty() { "" } else { "," },
                format_value(*b)
            );
        }
        let _ = writeln!(
            out,
            "{name}_bucket{{{labels}{}le=\"+Inf\"}} {}",
            if labels.is_empty() { "" } else { "," },
            self.count
        );
        let braced = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        let _ = writeln!(out, "{name}_sum{braced} {}", format_value(self.sum));
        let _ = writeln!(out, "{name}_count{braced} {}", self.count);
    }
}

/// One counter family: name, help text, and how to read it off a series.
type CounterDef = (&'static str, &'static str, fn(&Series) -> f64);

struct Series {
    requests: u64,
    errors: u64,
    cost_usd: f64,
    without_cost: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
    input_tokens: u64,
    output_tokens: u64,
    duration: Hist,
    ttft: Hist,
}

impl Series {
    fn new() -> Self {
        Self {
            requests: 0,
            errors: 0,
            cost_usd: 0.0,
            without_cost: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            input_tokens: 0,
            output_tokens: 0,
            duration: Hist::new(&TIME_BUCKETS_S),
            ttft: Hist::new(&TIME_BUCKETS_S),
        }
    }
}

struct Inner {
    providers: Interner,
    models: Interner,
    services: Interner,
    series: HashMap<(u32, u32, u32), Series>,
    folded: u64,
    /// Most recent numeric scores per evaluator.
    scores: HashMap<String, VecDeque<f64>>,
    scores_untracked: u64,
}

/// The live usage series. Cheap to share; all mutation is behind one short-held lock.
pub struct UsageMetrics {
    inner: Mutex<Inner>,
}

impl UsageMetrics {
    pub fn new(cfg: &UsageConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                providers: Interner::new(PROVIDER_CAP),
                models: Interner::new(cfg.model_cap.max(1)),
                services: Interner::new(SERVICE_CAP),
                series: HashMap::new(),
                folded: 0,
                scores: HashMap::new(),
                scores_untracked: 0,
            }),
        }
    }

    /// Record one committed batch. Non-LLM spans (retriever, tool, chain, …) are ignored.
    pub fn record(&self, spans: &[NormalizedSpan]) {
        let mut g = self.inner.lock().expect("usage lock");
        let inner = &mut *g;
        for s in spans {
            if !s.is_llm_span() {
                continue;
            }
            let mut folded = false;
            let p = inner
                .providers
                .intern(s.provider.as_deref().unwrap_or("unknown"), &mut folded);
            let m = inner
                .models
                .intern(s.model.as_deref().unwrap_or("unknown"), &mut folded);
            let sv = inner
                .services
                .intern(s.service_name.as_deref().unwrap_or("unknown"), &mut folded);
            let mut key = (p, m, sv);
            // One slot is reserved for the folded tuple, so the total never exceeds MAX_SERIES.
            if !inner.series.contains_key(&key) && inner.series.len() >= MAX_SERIES - 1 {
                key = (0, 0, 0);
                folded = true;
            }
            if folded {
                inner.folded += 1;
            }
            let e = inner.series.entry(key).or_insert_with(Series::new);
            e.requests += 1;
            if s.status_code == STATUS_ERROR {
                e.errors += 1;
            }
            match s.cost_usd {
                Some(c) if c.is_finite() => e.cost_usd += c,
                _ => e.without_cost += 1,
            }
            e.duration.observe(s.duration_ns() as f64 / 1e9);
            if let Some(t) = crate::latency::ttft_seconds(&s.raw_attributes) {
                e.ttft.observe(t);
            }
            e.input_tokens += s.tokens.prompt.unwrap_or(0);
            e.output_tokens += s.tokens.completion.unwrap_or(0);
            e.cache_read += s.tokens.cache_read.unwrap_or(0);
            e.cache_write += s.tokens.cache_write.unwrap_or(0);
            e.reasoning += s.tokens.reasoning.unwrap_or(0);
        }
    }

    /// Record numeric scores into the per-evaluator rolling windows. Run-level aggregates are
    /// skipped: mixing one aggregate into a window of per-item scores would skew the mean.
    pub fn record_scores(&self, scores: &[Score]) {
        let mut g = self.inner.lock().expect("usage lock");
        let inner = &mut *g;
        for sc in scores {
            let Some(v) = sc.num_value.filter(|v| v.is_finite()) else {
                continue;
            };
            if matches!(sc.target, ScoreTarget::Run(_)) || sc.agg_stats.is_some() {
                continue;
            }
            if !inner.scores.contains_key(&sc.name) && inner.scores.len() >= SCORE_NAMES_CAP {
                inner.scores_untracked += 1;
                continue;
            }
            let w = inner.scores.entry(sc.name.clone()).or_default();
            if w.len() >= SCORE_WINDOW {
                w.pop_front();
            }
            w.push_back(v);
        }
    }

    /// Append the Prometheus text for every usage series to `out`.
    pub fn render(&self, out: &mut String) {
        let g = self.inner.lock().expect("usage lock");
        let mut keys: Vec<&(u32, u32, u32)> = g.series.keys().collect();
        // Sorted by label text so the body is stable between scrapes.
        keys.sort_by_key(|k| {
            (
                g.providers.names[k.0 as usize].clone(),
                g.models.names[k.1 as usize].clone(),
                g.services.names[k.2 as usize].clone(),
            )
        });
        let labels = |k: &(u32, u32, u32)| {
            format!(
                "gen_ai_provider_name=\"{}\",gen_ai_request_model=\"{}\",service_name=\"{}\"",
                escape_label(&g.providers.names[k.0 as usize]),
                escape_label(&g.models.names[k.1 as usize]),
                escape_label(&g.services.names[k.2 as usize]),
            )
        };

        family(out, "evald_usage_series", "gauge",
            "Distinct (provider, model, service) label tuples currently tracked; bounded by a hard ceiling.");
        let _ = writeln!(out, "evald_usage_series {}", g.series.len());
        family(out, "evald_usage_labels_folded_total", "counter",
            "LLM spans whose provider, model or service label was folded into `other` because a cardinality cap was reached. Non-zero means raise --metrics-model-cap or shard the fleet.");
        let _ = writeln!(out, "evald_usage_labels_folded_total {}", g.folded);

        if !keys.is_empty() {
            let counters: [CounterDef; 9] = [
                ("evald_llm_requests_total",
                 "LLM spans accepted since process start (approximate under exporter retries).",
                 |s| s.requests as f64),
                ("evald_llm_request_errors_total",
                 "LLM spans accepted with an error status since process start.",
                 |s| s.errors as f64),
                ("evald_llm_cost_usd_total",
                 "Sum of cost_usd over accepted LLM spans that carry one (supplied or derived). Partial while evald_llm_spans_without_cost_total is rising.",
                 |s| s.cost_usd),
                ("evald_llm_spans_without_cost_total",
                 "Accepted LLM spans carrying no cost_usd; the cost counter understates spend by whatever these cost.",
                 |s| s.without_cost as f64),
                ("evald_llm_input_tokens_total",
                 "Input (prompt) tokens reported by accepted LLM spans, as the span reported them: whether this includes cached tokens depends on the instrumentation.",
                 |s| s.input_tokens as f64),
                ("evald_llm_output_tokens_total",
                 "Output (completion) tokens reported by accepted LLM spans.",
                 |s| s.output_tokens as f64),
                ("evald_llm_cache_read_tokens_total",
                 "Cache-read input tokens reported by accepted LLM spans.",
                 |s| s.cache_read as f64),
                ("evald_llm_cache_write_tokens_total",
                 "Cache-write (creation) input tokens reported by accepted LLM spans.",
                 |s| s.cache_write as f64),
                ("evald_llm_reasoning_tokens_total",
                 "Reasoning output tokens reported by accepted LLM spans.",
                 |s| s.reasoning as f64),
            ];
            for (name, help, get) in counters {
                family(out, name, "counter", help);
                for k in &keys {
                    let _ = writeln!(
                        out,
                        "{name}{{{}}} {}",
                        labels(k),
                        format_value(get(&g.series[*k]))
                    );
                }
            }

            family(out, "gen_ai_client_operation_duration_seconds", "histogram",
                "Wall time of accepted LLM spans (end - start), in seconds. Bounds follow the OpenTelemetry GenAI advisory boundaries, extended to 327.68 s.");
            for k in &keys {
                g.series[*k].duration.render(
                    out,
                    "gen_ai_client_operation_duration_seconds",
                    &labels(k),
                );
            }
            // Only series that actually observed a time to first token, and the family only
            // when at least one did: an empty histogram is ~19 lines of zeros per series, and
            // most traffic (non-streaming) never carries one.
            if keys.iter().any(|k| g.series[*k].ttft.count > 0) {
                family(out, "gen_ai_client_operation_time_to_first_chunk_seconds", "histogram",
                    "Time to first chunk of a streamed LLM response, in seconds, for spans that carry a time-to-first-token attribute. Spans without one are not observed and publish no series; evald never estimates it.");
                for k in keys.iter().filter(|k| g.series[**k].ttft.count > 0) {
                    g.series[*k].ttft.render(
                        out,
                        "gen_ai_client_operation_time_to_first_chunk_seconds",
                        &labels(k),
                    );
                }
            }
        }

        if !g.scores.is_empty() {
            let mut names: Vec<&String> = g.scores.keys().collect();
            names.sort();
            family(out, "evald_eval_score_mean", "gauge",
                "Mean of the most recent numeric scores per evaluator (window: evald_eval_score_window_samples, at most 1024), since process start. Run-level aggregates are excluded.");
            for n in &names {
                let w = &g.scores[*n];
                let mean = w.iter().sum::<f64>() / w.len() as f64;
                let _ = writeln!(
                    out,
                    "evald_eval_score_mean{{evaluator=\"{}\"}} {}",
                    escape_label(n),
                    format_value(mean)
                );
            }
            family(
                out,
                "evald_eval_score_window_samples",
                "gauge",
                "Scores currently in each evaluator's rolling window.",
            );
            for n in &names {
                let _ = writeln!(
                    out,
                    "evald_eval_score_window_samples{{evaluator=\"{}\"}} {}",
                    escape_label(n),
                    g.scores[*n].len()
                );
            }
        }
        if g.scores_untracked > 0 {
            family(
                out,
                "evald_eval_scores_untracked_total",
                "counter",
                "Numeric scores ignored because 64 evaluator names were already tracked.",
            );
            let _ = writeln!(
                out,
                "evald_eval_scores_untracked_total {}",
                g.scores_untracked
            );
        }
    }

    /// Number of tracked label tuples (tests and `/v1/stats`).
    pub fn series_len(&self) -> usize {
        self.inner.lock().expect("usage lock").series.len()
    }
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {}", escape_help(help));
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_span;
    use crate::{Store, StoreConfig};
    use std::sync::Arc;

    fn llm(id: &str, model: &str, dur_ms: u64) -> NormalizedSpan {
        let mut s = test_span("t", id, 1_000_000_000);
        s.oi_kind = Some("LLM".into());
        s.model = Some(model.into());
        s.provider = Some("openai".into());
        s.service_name = Some("chat".into());
        s.end_unix_nano = s.start_unix_nano + dur_ms * 1_000_000;
        s
    }

    fn cfg(model_cap: usize) -> UsageConfig {
        UsageConfig {
            enabled: true,
            model_cap,
        }
    }

    fn render(u: &UsageMetrics) -> String {
        let mut out = String::new();
        u.render(&mut out);
        out
    }

    type Sample = (String, Vec<(String, String)>, f64);

    /// A strict little parser for the text exposition format: every line is a comment or a
    /// `name{labels} value` sample with well-formed, correctly escaped labels; every family has
    /// exactly one TYPE before its first sample; histograms are cumulative and their `+Inf`
    /// bucket equals `_count`. Returns `(name, labels, value)` for each sample.
    fn parse_exposition(text: &str) -> Vec<Sample> {
        let sample =
            regex::Regex::new(r#"^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(.*)\})? (\S+)$"#).unwrap();
        let pair = regex::Regex::new(r#"([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)""#).unwrap();
        let mut types: HashMap<String, String> = HashMap::new();
        let mut out = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').expect("TYPE line");
                assert!(
                    types.insert(name.to_string(), kind.to_string()).is_none(),
                    "duplicate TYPE for {name}"
                );
                continue;
            }
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let c = sample
                .captures(line)
                .unwrap_or_else(|| panic!("malformed line: {line}"));
            let name = c[1].to_string();
            let mut labels = Vec::new();
            if let Some(l) = c.get(2) {
                let mut consumed = 0;
                for p in pair.captures_iter(l.as_str()) {
                    labels.push((p[1].to_string(), p[2].to_string()));
                    consumed += p[0].len();
                }
                let commas = labels.len().saturating_sub(1);
                assert_eq!(
                    consumed + commas,
                    l.as_str().len(),
                    "labels not fully parsed: {line}"
                );
            }
            let value = match &c[3] {
                "NaN" => f64::NAN,
                "+Inf" => f64::INFINITY,
                "-Inf" => f64::NEG_INFINITY,
                v => v.parse().unwrap_or_else(|_| panic!("bad value in: {line}")),
            };
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|suf| {
                    name.strip_suffix(suf)
                        .filter(|f| types.get(*f).map(String::as_str) == Some("histogram"))
                })
                .unwrap_or(&name)
                .to_string();
            assert!(types.contains_key(&family), "sample before TYPE: {line}");
            out.push((name, labels, value));
        }
        // Histogram invariants.
        type Key = (String, Vec<(String, String)>);
        let mut buckets: HashMap<Key, Vec<(f64, f64)>> = HashMap::new();
        let mut counts: HashMap<Key, f64> = HashMap::new();
        for (name, labels, v) in &out {
            let without_le: Vec<_> = labels.iter().filter(|(k, _)| k != "le").cloned().collect();
            if let Some(f) = name.strip_suffix("_bucket") {
                let le = labels
                    .iter()
                    .find(|(k, _)| k == "le")
                    .expect("bucket has le")
                    .1
                    .clone();
                let le = if le == "+Inf" {
                    f64::INFINITY
                } else {
                    le.parse().unwrap()
                };
                buckets
                    .entry((f.to_string(), without_le))
                    .or_default()
                    .push((le, *v));
            } else if let Some(f) = name.strip_suffix("_count") {
                counts.insert((f.to_string(), without_le), *v);
            }
        }
        for (key, mut b) in buckets {
            b.sort_by(|x, y| x.0.total_cmp(&y.0));
            assert!(
                b.windows(2).all(|w| w[0].1 <= w[1].1),
                "non-cumulative buckets: {key:?}"
            );
            assert_eq!(b.last().unwrap().0, f64::INFINITY);
            assert_eq!(
                b.last().unwrap().1,
                counts[&key],
                "+Inf != _count for {key:?}"
            );
        }
        out
    }

    fn value(samples: &[Sample], name: &str, model: &str) -> f64 {
        samples
            .iter()
            .find(|(n, l, _)| {
                n == name
                    && l.iter()
                        .any(|(k, v)| k == "gen_ai_request_model" && v == model)
            })
            .unwrap_or_else(|| panic!("no {name} for {model}"))
            .2
    }

    #[test]
    fn counters_histograms_and_the_scrape_are_consistent() {
        let u = UsageMetrics::new(&cfg(100));
        let mut a = llm("1", "gpt-4o", 300);
        a.cost_usd = Some(0.25);
        a.tokens.prompt = Some(100);
        a.tokens.completion = Some(20);
        a.tokens.cache_read = Some(40);
        a.tokens.cache_write = Some(5);
        a.tokens.reasoning = Some(7);
        let mut b = llm("2", "gpt-4o", 10_240); // exactly on a bucket bound (10.24 s)
        b.status_code = 2;
        b.raw_attributes.insert(
            "gen_ai.response.time_to_first_chunk".into(),
            serde_json::json!(0.5),
        );
        let mut retriever = test_span("t", "3", 1);
        retriever.oi_kind = Some("RETRIEVER".into());
        u.record(&[a, b, retriever]);

        let s = parse_exposition(&render(&u));
        assert_eq!(value(&s, "evald_llm_requests_total", "gpt-4o"), 2.0);
        assert_eq!(value(&s, "evald_llm_request_errors_total", "gpt-4o"), 1.0);
        assert!((value(&s, "evald_llm_cost_usd_total", "gpt-4o") - 0.25).abs() < 1e-12);
        assert_eq!(
            value(&s, "evald_llm_spans_without_cost_total", "gpt-4o"),
            1.0
        );
        assert_eq!(value(&s, "evald_llm_input_tokens_total", "gpt-4o"), 100.0);
        assert_eq!(value(&s, "evald_llm_output_tokens_total", "gpt-4o"), 20.0);
        assert_eq!(
            value(&s, "evald_llm_cache_read_tokens_total", "gpt-4o"),
            40.0
        );
        assert_eq!(
            value(&s, "evald_llm_cache_write_tokens_total", "gpt-4o"),
            5.0
        );
        assert_eq!(value(&s, "evald_llm_reasoning_tokens_total", "gpt-4o"), 7.0);
        assert_eq!(
            value(
                &s,
                "gen_ai_client_operation_duration_seconds_count",
                "gpt-4o"
            ),
            2.0
        );
        assert!(
            (value(&s, "gen_ai_client_operation_duration_seconds_sum", "gpt-4o") - 10.54).abs()
                < 1e-9
        );
        // 10.24 s sits ON the le="10.24" bound, so it is inside that bucket, not the next.
        let le = |name: &str, le: &str| {
            s.iter()
                .find(|(n, l, _)| n == name && l.iter().any(|(k, v)| k == "le" && v == le))
                .unwrap()
                .2
        };
        assert_eq!(
            le("gen_ai_client_operation_duration_seconds_bucket", "5.12"),
            1.0
        );
        assert_eq!(
            le("gen_ai_client_operation_duration_seconds_bucket", "10.24"),
            2.0
        );
        // Only the span carrying a TTFT is observed; the other is never estimated.
        assert_eq!(
            value(
                &s,
                "gen_ai_client_operation_time_to_first_chunk_seconds_count",
                "gpt-4o"
            ),
            1.0
        );
        // The retriever produced no series of its own.
        assert_eq!(u.series_len(), 1);
    }

    #[test]
    fn ten_thousand_models_stay_within_the_cap() {
        let u = UsageMetrics::new(&cfg(100));
        let spans: Vec<_> = (0..10_000)
            .map(|i| llm(&format!("{i}"), &format!("model-{i}"), 5))
            .collect();
        u.record(&spans);
        assert_eq!(u.series_len(), 101, "100 models + `other`");
        let text = render(&u);
        let s = parse_exposition(&text);
        assert_eq!(value(&s, "evald_llm_requests_total", "other"), 9_900.0);
        let folded = s
            .iter()
            .find(|(n, _, _)| n == "evald_usage_labels_folded_total")
            .unwrap()
            .2;
        assert_eq!(folded, 9_900.0);
        // ~28 lines per series (9 counters + the 19-line duration histogram): the worst case a
        // cap-hit fleet ever pays, and still well under a scraper's default body limit.
        assert!(
            text.len() < 600_000,
            "scrape body must stay small, was {}",
            text.len()
        );
        // The interner never grew past its cap.
        let g = u.inner.lock().unwrap();
        assert_eq!(g.models.names.len(), 101);
        assert_eq!(g.models.ids.len(), 100);
    }

    #[test]
    fn the_tuple_ceiling_holds_even_when_every_dimension_varies() {
        let u = UsageMetrics::new(&cfg(1_000));
        let mut spans = Vec::new();
        for i in 0..8_000u32 {
            let mut s = llm(&format!("{i}"), &format!("m{}", i % 700), 1);
            s.provider = Some(format!("p{}", i % 13));
            s.service_name = Some(format!("s{}", i % 29));
            spans.push(s);
        }
        u.record(&spans);
        assert!(u.series_len() <= MAX_SERIES, "{} series", u.series_len());
    }

    #[test]
    fn counters_are_exact_under_concurrent_recording() {
        let u = Arc::new(UsageMetrics::new(&cfg(100)));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let u = u.clone();
                std::thread::spawn(move || {
                    for b in 0..200 {
                        let batch: Vec<_> = (0..10)
                            .map(|i| llm(&format!("{t}-{b}-{i}"), "gpt-4o", 100))
                            .collect();
                        u.record(&batch);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let s = parse_exposition(&render(&u));
        assert_eq!(value(&s, "evald_llm_requests_total", "gpt-4o"), 16_000.0);
        assert_eq!(
            value(
                &s,
                "gen_ai_client_operation_duration_seconds_count",
                "gpt-4o"
            ),
            16_000.0
        );
    }

    #[test]
    fn score_windows_are_bounded_and_track_the_recent_mean() {
        use crate::{DataType, ScoreSource};
        let u = UsageMetrics::new(&cfg(100));
        let score = |name: &str, v: f64, target: ScoreTarget| Score {
            id: format!("{name}-{v}"),
            target,
            name: name.into(),
            num_value: Some(v),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::default(),
            comment: None,
            config_id: None,
            agg_stats: None,
            ts_unix_nano: 0,
        };
        // 3,000 scores of 0 then 1,024 of 1: the window keeps only the last 1,024.
        let mut v: Vec<_> = (0..3_000)
            .map(|_| score("faith", 0.0, ScoreTarget::Span("s".into())))
            .collect();
        v.extend((0..1_024).map(|_| score("faith", 1.0, ScoreTarget::Span("s".into()))));
        v.push(score("faith", 0.0, ScoreTarget::Run("r".into()))); // aggregates are skipped
        u.record_scores(&v);
        let s = parse_exposition(&render(&u));
        let get = |n: &str| s.iter().find(|(m, _, _)| m == n).unwrap().2;
        assert_eq!(get("evald_eval_score_window_samples"), 1024.0);
        assert_eq!(get("evald_eval_score_mean"), 1.0);

        // Evaluator names are capped.
        let many: Vec<_> = (0..100)
            .map(|i| score(&format!("e{i}"), 1.0, ScoreTarget::Trace("t".into())))
            .collect();
        u.record_scores(&many);
        let text = render(&u);
        let s = parse_exposition(&text);
        assert_eq!(
            s.iter()
                .filter(|(n, _, _)| n == "evald_eval_score_mean")
                .count(),
            64
        );
        assert!(
            text.contains("evald_eval_scores_untracked_total 37"),
            "{text}"
        );
    }

    fn one_span(id: &str) -> Vec<NormalizedSpan> {
        let mut s = llm(id, "gpt-4o", 50);
        s.cost_usd = Some(0.5);
        vec![s]
    }

    #[tokio::test]
    async fn accepted_spans_are_counted_and_a_wal_replay_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let mk = || {
            Store::open(
                dir.path(),
                StoreConfig {
                    compact_interval: None,
                    ..StoreConfig::default()
                },
            )
            .unwrap()
        };
        let store = mk();
        store.append(one_span("a")).await.unwrap();
        store.append(one_span("b")).await.unwrap();
        let s = parse_exposition(&crate::metrics::render(&store));
        assert_eq!(value(&s, "evald_llm_requests_total", "gpt-4o"), 2.0);

        // Restart: the two spans are recovered from the WAL and are queryable, but they were
        // not *accepted* by this process, so the counters start at zero.
        drop(store);
        let store = mk();
        assert_eq!(
            store.query(None, 100).unwrap().len(),
            2,
            "recovered from the WAL"
        );
        let scraped = crate::metrics::render(&store);
        assert!(!scraped.contains("evald_llm_requests_total"), "{scraped}");
        parse_exposition(&scraped);
        // ...and a genuinely new span counts as one.
        store.append(one_span("c")).await.unwrap();
        let s = parse_exposition(&crate::metrics::render(&store));
        assert_eq!(value(&s, "evald_llm_requests_total", "gpt-4o"), 1.0);
    }

    #[tokio::test]
    async fn the_switch_removes_every_usage_series_and_keeps_the_health_series() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                usage: UsageConfig {
                    enabled: false,
                    model_cap: 100,
                },
                ..StoreConfig::default()
            },
        )
        .unwrap();
        store.append(one_span("a")).await.unwrap();
        let scraped = crate::metrics::render(&store);
        assert!(
            !scraped.contains("evald_llm_"),
            "disabled must publish nothing"
        );
        assert!(!scraped.contains("gen_ai_client_"));
        assert!(
            scraped.contains("evald_spans_ingested_total 1"),
            "health series remain"
        );
        parse_exposition(&scraped);
    }
}
