//! `gen_ai.evaluation.result` — the OpenTelemetry GenAI evaluation-result event, in and out.
//!
//! **In.** A span event named [`EVENT_NAME`] inside an OTLP traces export becomes an ordinary
//! [`Score`] targeting the span the event is attached to, so an application (or an evaluation
//! library) that already emits the standard event needs no evald-specific code. Every ingest
//! door (HTTP protobuf, OTLP-JSON, gRPC) decodes into the same `ExportTraceServiceRequest`, so
//! [`extract`] is called once per door next to `normalize_request` and nowhere else.
//!
//! **Out.** [`export_line`] renders a stored score back as the event, one JSON object per
//! line (`evald scores export --format gen_ai-event`).
//!
//! # Pinned specification
//!
//! The event is at **Development** stability, so the shape below is pinned to a specific text
//! ([`SPEC_PIN`]) and is expected to move: `gen_ai.evaluation.name` (required),
//! `gen_ai.evaluation.score.value` (double), `gen_ai.evaluation.score.label`,
//! `gen_ai.evaluation.explanation`, `error.type`, `gen_ai.response.id`. The spec says the event
//! SHOULD be parented to the evaluated GenAI operation span, and carry `gen_ai.response.id` only
//! when the span is not available.
//!
//! # What is and is not ingested
//!
//! * **Span events** in a traces export: yes. The target is the span carrying the event.
//! * **Log-record events** (the OTLP *logs* signal, which is how the spec's SDKs emit events
//!   today): **not ingested**, evald has no logs endpoint. Emit the evaluation as a span event,
//!   or post it to `POST /v1/scores`.
//! * `gen_ai.response.id`: not stored. A span event is already attached to its span, which is
//!   the correlation the spec falls back to the id for.
//!
//! # Guarantees
//!
//! * **Idempotent under exporter retries.** The score id is a hash of `(trace, span, evaluation
//!   name, event time)`, and the score store upserts by id, so a re-sent batch overwrites rather
//!   than duplicates.
//! * **Never fails the span ingest.** A malformed event (no evaluation name, or neither a
//!   numeric value, a label, nor an `error.type`) is dropped and counted
//!   (`evald_eval_events_malformed_total`); the spans in the same request are stored regardless.
//! * **Bounded.** Names, labels, explanations and error types are length-capped; a non-numeric or
//!   non-finite value is ignored and the label, if any, is kept.
//! * **Off the fast path.** A span with no events costs one `Vec::is_empty` check, and an event
//!   with another name costs one string comparison. Nothing is allocated for either.

use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::trace::v1::{span::Event, Span};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::model::{DataType, Score, ScoreSource, ScoreTarget};
use crate::normalize::{attr_f64, attr_str};

/// The event name the spec fixes.
pub const EVENT_NAME: &str = "gen_ai.evaluation.result";

/// The specification text this implementation follows. Bump it (and re-check the attribute
/// names below) when the spec moves.
pub const SPEC_PIN: &str = "OpenTelemetry semantic-conventions-genai @ cc07f72 (2026-09-21), \
     event gen_ai.evaluation.result, semconv v1.44.0, status: Development";

const NAME: &str = "gen_ai.evaluation.name";
const VALUE: &str = "gen_ai.evaluation.score.value";
const LABEL: &str = "gen_ai.evaluation.score.label";
const EXPLANATION: &str = "gen_ai.evaluation.explanation";
const ERROR_TYPE: &str = "error.type";

const MAX_NAME: usize = 256;
const MAX_LABEL: usize = 256;
const MAX_EXPLANATION: usize = 4096;
const MAX_ERROR_TYPE: usize = 256;

static INGESTED: AtomicU64 = AtomicU64::new(0);
static MALFORMED: AtomicU64 = AtomicU64::new(0);

/// `(events turned into scores, malformed events dropped)` since process start.
pub fn counts() -> (u64, u64) {
    (
        INGESTED.load(Ordering::Relaxed),
        MALFORMED.load(Ordering::Relaxed),
    )
}

/// Every `gen_ai.evaluation.result` span event in the request, as a [`Score`]. Malformed events
/// are counted and skipped, never an error.
pub fn extract(req: &ExportTraceServiceRequest) -> Vec<Score> {
    let mut out = Vec::new();
    let mut bad = 0u64;
    for resource_spans in &req.resource_spans {
        for scope_spans in &resource_spans.scope_spans {
            for span in &scope_spans.spans {
                if span.events.is_empty() {
                    continue;
                }
                for event in &span.events {
                    if event.name != EVENT_NAME {
                        continue;
                    }
                    match to_score(span, event) {
                        Some(score) => out.push(score),
                        None => bad += 1,
                    }
                }
            }
        }
    }
    if !out.is_empty() {
        INGESTED.fetch_add(out.len() as u64, Ordering::Relaxed);
    }
    if bad > 0 {
        MALFORMED.fetch_add(bad, Ordering::Relaxed);
        tracing::warn!(
            malformed = bad,
            "dropped malformed {EVENT_NAME} event(s): each needs {NAME} and one of a numeric \
             {VALUE}, a {LABEL} or an {ERROR_TYPE}"
        );
    }
    out
}

fn to_score(span: &Span, event: &Event) -> Option<Score> {
    let a = &event.attributes;
    let name = cap(attr_str(a, NAME)?.trim(), MAX_NAME);
    if name.is_empty() {
        return None;
    }
    // A value that is not a finite number is ignored (the label, if any, is kept).
    let value = attr_f64(a, VALUE).filter(|v| v.is_finite());
    let label = attr_str(a, LABEL)
        .map(|l| cap(l.trim(), MAX_LABEL))
        .filter(|l| !l.is_empty());
    let explanation = attr_str(a, EXPLANATION)
        .map(|e| cap(&e, MAX_EXPLANATION))
        .filter(|e| !e.is_empty());
    let error_type = attr_str(a, ERROR_TYPE)
        .map(|e| cap(e.trim(), MAX_ERROR_TYPE))
        .filter(|e| !e.is_empty());

    let (data_type, num_value, str_value) = match (value, label, &error_type) {
        (Some(v), l, _) => (DataType::Numeric, Some(v), l),
        (None, Some(l), _) => (DataType::Categorical, None, Some(l)),
        // An evaluation that itself errored: keep it visible instead of dropping it.
        (None, None, Some(_)) => (DataType::Categorical, None, Some("error".to_string())),
        (None, None, None) => return None,
    };
    let comment = match (explanation, &error_type) {
        (Some(e), Some(t)) => Some(format!("{e} | error.type={t}")),
        (Some(e), None) => Some(e),
        (None, Some(t)) => Some(format!("error.type={t}")),
        (None, None) => None,
    };

    let trace_id = hex::encode(&span.trace_id);
    let (target, span_id) = if span.span_id.is_empty() {
        if trace_id.is_empty() {
            return None;
        }
        (ScoreTarget::Trace(trace_id.clone()), String::new())
    } else {
        let s = hex::encode(&span.span_id);
        (ScoreTarget::Span(s.clone()), s)
    };
    let ts = if event.time_unix_nano > 0 {
        event.time_unix_nano
    } else {
        span.end_time_unix_nano
    };

    Some(Score {
        id: score_id(&trace_id, &span_id, &name, event.time_unix_nano),
        target,
        name,
        num_value,
        str_value,
        data_type,
        source: ScoreSource::Eval,
        comment,
        config_id: None,
        agg_stats: None,
        ts_unix_nano: ts,
    })
}

/// Deterministic id: the same event re-sent (an exporter retry) lands on the same score.
fn score_id(trace_id: &str, span_id: &str, name: &str, time_unix_nano: u64) -> String {
    let mut h = Sha256::new();
    for part in [trace_id, span_id, name] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    h.update(time_unix_nano.to_be_bytes());
    format!("gaie-{}", hex::encode(&h.finalize()[..16]))
}

/// Truncate to at most `max` bytes on a character boundary.
fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// One stored score as the event, for `evald scores export --format gen_ai-event`: a JSON object
/// per line carrying the event name, time, the trace/span it belongs to, and the spec's
/// attribute map. Scores that do not belong to a span or trace (run aggregates, sessions) have
/// no operation to parent to and return `None`.
pub fn export_line(score: &Score) -> Option<String> {
    let (trace_id, span_id) = match &score.target {
        ScoreTarget::Span(s) => (None, Some(s.as_str())),
        ScoreTarget::Trace(t) => (Some(t.as_str()), None),
        _ => return None,
    };
    let mut attrs = serde_json::Map::new();
    attrs.insert(NAME.into(), json!(score.name));
    if let Some(v) = score.num_value {
        attrs.insert(VALUE.into(), json!(v));
    }
    if let Some(l) = &score.str_value {
        attrs.insert(LABEL.into(), json!(l));
    }
    if let Some(c) = &score.comment {
        attrs.insert(EXPLANATION.into(), json!(c));
    }
    let mut line = serde_json::Map::new();
    line.insert("name".into(), json!(EVENT_NAME));
    line.insert("time_unix_nano".into(), json!(score.ts_unix_nano));
    if let Some(t) = trace_id {
        line.insert("trace_id".into(), json!(t));
    }
    if let Some(s) = span_id {
        line.insert("span_id".into(), json!(s));
    }
    line.insert("attributes".into(), Value::Object(attrs));
    serde_json::to_string(&Value::Object(line)).ok()
}

/// The newest `limit` exportable scores (optionally only those named `name`), as event lines in
/// chronological order. Run aggregates and session scores are skipped (see [`export_line`]).
pub fn export_lines(
    store: &crate::Store,
    name: Option<&str>,
    limit: usize,
) -> std::io::Result<Vec<String>> {
    // `list_scores` is newest-first; take the newest `limit`, then flip to oldest-first.
    let mut lines: Vec<String> = store
        .list_scores(usize::MAX)?
        .iter()
        .filter(|s| name.is_none_or(|n| s.name == n))
        .filter_map(export_line)
        .take(limit)
        .collect();
    lines.reverse();
    Ok(lines)
}

/// `evald scores export --format gen_ai-event`: open the data-dir (like every offline command,
/// this takes its lock, so it cannot run against a live `serve` on the same dir) and print the
/// events, one JSON object per line, to stdout. Returns how many were written.
pub fn export_command(
    data_dir: &std::path::Path,
    name: Option<&str>,
    limit: usize,
) -> anyhow::Result<usize> {
    use std::io::Write;
    let store = crate::Store::open(
        data_dir,
        crate::StoreConfig {
            compact_interval: None,
            ..crate::StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let lines = export_lines(&store, name, limit)?;
    let mut out = std::io::stdout().lock();
    for line in &lines {
        writeln!(out, "{line}")?;
    }
    Ok(lines.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{any_value::Value as AnyVal, AnyValue, KeyValue};
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans};

    fn kv(key: &str, v: AnyVal) -> KeyValue {
        KeyValue {
            key: key.into(),
            value: Some(AnyValue { value: Some(v) }),
            ..Default::default()
        }
    }
    fn s(key: &str, v: &str) -> KeyValue {
        kv(key, AnyVal::StringValue(v.into()))
    }
    fn d(key: &str, v: f64) -> KeyValue {
        kv(key, AnyVal::DoubleValue(v))
    }

    fn event(time: u64, attrs: Vec<KeyValue>) -> Event {
        Event {
            time_unix_nano: time,
            name: EVENT_NAME.into(),
            attributes: attrs,
            ..Default::default()
        }
    }

    pub(crate) fn request_with(events: Vec<Event>) -> ExportTraceServiceRequest {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        trace_id: vec![0x11; 16],
                        span_id: vec![0x22; 8],
                        name: "chat".into(),
                        end_time_unix_nano: 9_000,
                        events,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    #[test]
    fn value_and_label_become_a_numeric_score_keyed_to_the_span() {
        let req = request_with(vec![event(
            5_000,
            vec![
                s(NAME, "Relevance"),
                d(VALUE, 4.0),
                s(LABEL, "relevant"),
                s(EXPLANATION, "on topic"),
            ],
        )]);
        let scores = extract(&req);
        assert_eq!(scores.len(), 1);
        let sc = &scores[0];
        assert_eq!(sc.target, ScoreTarget::Span("22".repeat(8)));
        assert_eq!(sc.name, "Relevance");
        assert_eq!(sc.num_value, Some(4.0));
        assert_eq!(sc.str_value.as_deref(), Some("relevant"));
        assert_eq!(sc.comment.as_deref(), Some("on topic"));
        assert_eq!(sc.data_type, DataType::Numeric);
        assert_eq!(sc.source, ScoreSource::Eval);
        assert_eq!(sc.ts_unix_nano, 5_000);
    }

    #[test]
    fn value_only_and_label_only_events() {
        let req = request_with(vec![
            event(1, vec![s(NAME, "a"), d(VALUE, 0.5)]),
            event(2, vec![s(NAME, "b"), s(LABEL, "pass")]),
        ]);
        let scores = extract(&req);
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[0].data_type, DataType::Numeric);
        assert_eq!(scores[0].str_value, None);
        assert_eq!(scores[1].data_type, DataType::Categorical);
        assert_eq!(scores[1].num_value, None);
        assert_eq!(scores[1].str_value.as_deref(), Some("pass"));
    }

    #[test]
    fn non_numeric_value_is_ignored_and_the_label_kept() {
        let req = request_with(vec![event(
            1,
            vec![s(NAME, "a"), s(VALUE, "not a number"), s(LABEL, "correct")],
        )]);
        let scores = extract(&req);
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].num_value, None);
        assert_eq!(scores[0].data_type, DataType::Categorical);
        assert_eq!(scores[0].str_value.as_deref(), Some("correct"));
        // NaN / infinity are not numbers either.
        let req = request_with(vec![event(
            1,
            vec![s(NAME, "a"), d(VALUE, f64::NAN), s(LABEL, "x")],
        )]);
        assert_eq!(extract(&req)[0].num_value, None);
    }

    #[test]
    fn numeric_string_and_int_values_are_accepted() {
        let req = request_with(vec![
            event(1, vec![s(NAME, "a"), s(VALUE, "0.25")]),
            event(2, vec![s(NAME, "b"), kv(VALUE, AnyVal::IntValue(3))]),
        ]);
        let scores = extract(&req);
        assert_eq!(scores[0].num_value, Some(0.25));
        assert_eq!(scores[1].num_value, Some(3.0));
    }

    #[test]
    fn an_errored_evaluation_is_kept_visible() {
        let req = request_with(vec![event(
            1,
            vec![
                s(NAME, "a"),
                s(ERROR_TYPE, "timeout"),
                s(EXPLANATION, "slow"),
            ],
        )]);
        let sc = &extract(&req)[0];
        assert_eq!(sc.str_value.as_deref(), Some("error"));
        assert_eq!(sc.comment.as_deref(), Some("slow | error.type=timeout"));
    }

    #[test]
    fn malformed_events_are_dropped_and_counted_without_affecting_others() {
        let before = counts().1;
        let req = request_with(vec![
            event(1, vec![d(VALUE, 1.0)]),                // no evaluation name
            event(2, vec![s(NAME, "  ")]),                // blank name
            event(3, vec![s(NAME, "x")]),                 // nothing to record
            event(4, vec![s(NAME, "ok"), d(VALUE, 1.0)]), // fine
        ]);
        let scores = extract(&req);
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].name, "ok");
        assert!(counts().1 >= before + 3);
    }

    #[test]
    fn other_events_and_spans_without_events_are_ignored() {
        let mut other = event(1, vec![s(NAME, "x"), d(VALUE, 1.0)]);
        other.name = "exception".into();
        assert!(extract(&request_with(vec![other])).is_empty());
        assert!(extract(&request_with(vec![])).is_empty());
    }

    #[test]
    fn the_same_event_resent_gets_the_same_id() {
        let mk = || request_with(vec![event(7, vec![s(NAME, "a"), d(VALUE, 1.0)])]);
        assert_eq!(extract(&mk())[0].id, extract(&mk())[0].id);
        // A different time or a different evaluation is a different score.
        let other = request_with(vec![event(8, vec![s(NAME, "a"), d(VALUE, 1.0)])]);
        assert_ne!(extract(&mk())[0].id, extract(&other)[0].id);
        let other = request_with(vec![event(7, vec![s(NAME, "b"), d(VALUE, 1.0)])]);
        assert_ne!(extract(&mk())[0].id, extract(&other)[0].id);
    }

    #[test]
    fn strings_are_capped_on_a_char_boundary() {
        let long = "é".repeat(MAX_EXPLANATION); // 2 bytes each: over the cap
        let req = request_with(vec![event(
            1,
            vec![
                s(NAME, &"n".repeat(1000)),
                d(VALUE, 1.0),
                s(EXPLANATION, &long),
            ],
        )]);
        let sc = &extract(&req)[0];
        assert_eq!(sc.name.len(), MAX_NAME);
        let c = sc.comment.as_deref().unwrap();
        assert!(c.len() <= MAX_EXPLANATION);
        assert!(c.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn a_zero_event_time_falls_back_to_the_span_end() {
        let req = request_with(vec![event(0, vec![s(NAME, "a"), d(VALUE, 1.0)])]);
        assert_eq!(extract(&req)[0].ts_unix_nano, 9_000);
    }

    #[test]
    fn export_round_trips_the_spec_attributes() {
        let req = request_with(vec![event(
            5_000,
            vec![
                s(NAME, "Relevance"),
                d(VALUE, 4.0),
                s(LABEL, "relevant"),
                s(EXPLANATION, "on topic"),
            ],
        )]);
        let line = export_line(&extract(&req)[0]).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["name"], EVENT_NAME);
        assert_eq!(v["span_id"], "22".repeat(8));
        assert_eq!(v["time_unix_nano"], 5_000);
        assert_eq!(v["attributes"][NAME], "Relevance");
        assert_eq!(v["attributes"][VALUE], 4.0);
        assert_eq!(v["attributes"][LABEL], "relevant");
        assert_eq!(v["attributes"][EXPLANATION], "on topic");
    }

    #[test]
    fn run_aggregates_and_sessions_are_not_exported() {
        let mut sc = extract(&request_with(vec![event(
            1,
            vec![s(NAME, "a"), d(VALUE, 1.0)],
        )]))
        .remove(0);
        sc.target = ScoreTarget::Run("r".into());
        assert!(export_line(&sc).is_none());
        sc.target = ScoreTarget::Session("s".into());
        assert!(export_line(&sc).is_none());
    }

    #[tokio::test]
    async fn export_lines_are_chronological_filtered_and_span_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::Store::open(
            dir.path(),
            crate::StoreConfig {
                compact_interval: None,
                ..crate::StoreConfig::default()
            },
        )
        .unwrap();
        let mut scores = extract(&request_with(vec![
            event(10, vec![s(NAME, "a"), d(VALUE, 1.0)]),
            event(20, vec![s(NAME, "b"), d(VALUE, 2.0)]),
            event(30, vec![s(NAME, "a"), d(VALUE, 3.0)]),
        ]));
        // A run aggregate is stored too but has no operation to parent to.
        let mut agg = scores[0].clone();
        agg.id = "r:agg:a".into();
        agg.target = ScoreTarget::Run("r".into());
        scores.push(agg);
        store.put_scores(&scores).unwrap();

        let all = export_lines(&store, None, 100).unwrap();
        assert_eq!(all.len(), 3, "the run aggregate must not be exported");
        let times: Vec<u64> = all
            .iter()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["time_unix_nano"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(times, vec![10, 20, 30], "oldest first");

        assert_eq!(export_lines(&store, Some("a"), 100).unwrap().len(), 2);
        // `limit` keeps the newest.
        let newest = export_lines(&store, None, 1).unwrap();
        assert!(newest[0].contains("\"time_unix_nano\":30"));
    }

    /// The pin is part of the contract: docs quote it, so it must not silently rot.
    #[test]
    fn the_spec_pin_names_a_commit_and_the_stability() {
        assert!(SPEC_PIN.contains("cc07f72") && SPEC_PIN.contains("Development"));
    }
}
