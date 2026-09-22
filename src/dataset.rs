//! Curate an eval dataset from captured spans (top-20 #20) — the production→eval loop.
//!
//! `evald dataset from-spans` selects stored spans and writes a JSONL dataset that
//! [`crate::eval::load_dataset`] can replay: each span with a captured `output.value` becomes one
//! row (`input` from `input.value`, `output` = the captured production output to score,
//! `span_id`/`trace_id` for provenance, plus a `metadata` object). This is what closes the loop
//! between the trace store and the offline eval-regression runner — "this trace looks bad → add it
//! to a dataset → gate future builds on it" — which every eval-lane competitor ships and evald
//! otherwise lacked (its runner could only read hand-authored JSONL).

use std::io::Write;
use std::path::Path;

use serde::Serialize;
use serde_json::json;

use crate::{NormalizedSpan, Store};

/// Which spans to curate.
pub struct FromSpansFilter {
    /// Only spans of this trace (else all recent spans).
    pub trace_id: Option<String>,
    /// Only spans of this model.
    pub model: Option<String>,
    /// Max spans to scan (most-recent-first).
    pub limit: usize,
}

/// One dataset row, serialized to match [`crate::eval::DatasetItem`] on read-back. `output` is
/// required by the eval runner (it is the text evaluators score), so a span without a captured
/// output can't form a row.
#[derive(Serialize)]
struct DatasetRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<String>,
    output: String,
    span_id: Option<String>,
    trace_id: Option<String>,
    metadata: serde_json::Value,
}

/// Turn spans into JSONL. Pure (no IO) so it is directly testable. Returns
/// `(jsonl, rows_written, skipped_no_output)`. A span is skipped (counted) when it matches the model
/// filter but has no non-empty `output.value`; a span filtered out by model is not counted as skipped.
pub fn rows_from_spans(
    spans: &[NormalizedSpan],
    model_filter: Option<&str>,
) -> (String, usize, usize) {
    let mut jsonl = String::new();
    let mut written = 0usize;
    let mut skipped = 0usize;
    for s in spans {
        if let Some(m) = model_filter {
            if s.model.as_deref() != Some(m) {
                continue;
            }
        }
        let output = match &s.output_value {
            Some(o) if !o.is_empty() => o.clone(),
            _ => {
                skipped += 1;
                continue;
            }
        };
        let row = DatasetRow {
            input: s.input_value.clone(),
            output,
            span_id: Some(s.span_id.clone()),
            trace_id: Some(s.trace_id.clone()),
            metadata: json!({
                "model": s.model,
                "provider": s.provider,
                "service_name": s.service_name,
                "session_id": s.session_id,
                "start_unix_nano": s.start_unix_nano,
            }),
        };
        if let Ok(line) = serde_json::to_string(&row) {
            jsonl.push_str(&line);
            jsonl.push('\n');
            written += 1;
        }
    }
    (jsonl, written, skipped)
}

/// Query the store and write a JSONL dataset to `out` (or stdout when `None`). Returns
/// `(rows_written, skipped_no_output)`.
pub fn from_spans(
    store: &Store,
    filter: &FromSpansFilter,
    out: Option<&Path>,
) -> anyhow::Result<(usize, usize)> {
    let mut spans = store.query(filter.trace_id.as_deref(), filter.limit)?;
    // Blob offload is an internal storage optimization (bloated fields get replaced with an
    // `evald-blob:<key>` reference on write) — resolve it back to real content here, before
    // `rows_from_spans`, so the dataset holds the text evaluators actually score, not opaque
    // blob ids that happen to look like input/output.
    for s in &mut spans {
        resolve_blob_ref(store, &mut s.input_value);
        resolve_blob_ref(store, &mut s.output_value);
    }
    let (jsonl, written, skipped) = rows_from_spans(&spans, filter.model.as_deref());
    match out {
        Some(path) => std::fs::write(path, jsonl)?,
        None => {
            let mut stdout = std::io::stdout();
            stdout.write_all(jsonl.as_bytes())?;
            stdout.flush().ok();
        }
    }
    Ok((written, skipped))
}

/// If `field` holds an `evald-blob:<key>` reference (see [`crate::blob::BLOB_REF_PREFIX`]),
/// replace it in place with the resolved blob content. An unresolvable reference (blob deleted
/// by retention, store error, non-UTF-8 bytes) clears the field rather than leaving the raw
/// reference — `rows_from_spans` then treats it as absent (skipped for `output`, omitted for
/// `input`) instead of silently emitting an opaque id as if it were real span content.
fn resolve_blob_ref(store: &Store, field: &mut Option<String>) {
    let Some(v) = field.as_deref() else { return };
    if !v.starts_with(crate::blob::BLOB_REF_PREFIX) {
        return;
    }
    *field = store
        .get_blob(v)
        .ok()
        .flatten()
        .and_then(|bytes| String::from_utf8(bytes).ok());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Dialect, Tokens};
    use std::collections::BTreeMap;

    fn span(
        span_id: &str,
        model: Option<&str>,
        input: Option<&str>,
        output: Option<&str>,
    ) -> NormalizedSpan {
        NormalizedSpan {
            dialect: Dialect::OpenInference,
            trace_id: "aa".repeat(16),
            span_id: span_id.to_string(),
            parent_span_id: None,
            name: "llm.call".into(),
            otel_kind: 3,
            oi_kind: Some("LLM".into()),
            start_unix_nano: 100,
            end_unix_nano: 200,
            status_code: 0,
            status_message: None,
            model: model.map(str::to_string),
            provider: Some("openai".into()),
            tokens: Tokens::default(),
            cost_usd: None,
            input_value: input.map(str::to_string),
            output_value: output.map(str::to_string),
            session_id: None,
            user_id: None,
            service_name: Some("svc".into()),
            scope_name: None,
            scope_version: None,
            raw_attributes: BTreeMap::new(),
        }
    }

    #[test]
    fn rows_are_written_only_for_spans_with_output() {
        let spans = vec![
            span("01", Some("gpt-4o"), Some("hi?"), Some("hello!")),
            span("02", Some("gpt-4o"), Some("no output here"), None), // skipped
            span("03", Some("gpt-4o"), Some("empty"), Some("")),      // skipped (empty)
        ];
        let (jsonl, written, skipped) = rows_from_spans(&spans, None);
        assert_eq!(written, 1);
        assert_eq!(skipped, 2);
        // The one row is valid JSONL with the eval-runner fields.
        let v: serde_json::Value = serde_json::from_str(jsonl.trim()).unwrap();
        assert_eq!(v["input"], "hi?");
        assert_eq!(v["output"], "hello!");
        assert_eq!(v["span_id"], "01");
        assert_eq!(v["metadata"]["model"], "gpt-4o");
    }

    #[test]
    fn model_filter_excludes_other_models_without_counting_them_skipped() {
        let spans = vec![
            span("01", Some("gpt-4o"), Some("a"), Some("A")),
            span("02", Some("claude"), Some("b"), Some("B")),
            span("03", Some("gpt-4o"), Some("c"), None), // matches filter, no output → skipped
        ];
        let (jsonl, written, skipped) = rows_from_spans(&spans, Some("gpt-4o"));
        assert_eq!(written, 1); // only 01 (claude filtered out, 03 has no output)
        assert_eq!(skipped, 1); // 03; the claude span is filtered, not "skipped"
        assert!(jsonl.contains("\"span_id\":\"01\""));
        assert!(!jsonl.contains("claude"));
    }

    #[test]
    fn produced_jsonl_round_trips_through_the_eval_loader() {
        // The whole point: what we write must load back as eval DatasetItems.
        let spans = vec![
            span("01", Some("gpt-4o"), Some("q1"), Some("a1")),
            span("02", Some("gpt-4o"), Some("q2"), Some("a2")),
        ];
        let (jsonl, written, _) = rows_from_spans(&spans, None);
        assert_eq!(written, 2);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ds.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        let items = crate::eval::load_dataset(&path).expect("dataset must load in the eval runner");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].output, "a1");
        assert_eq!(items[0].input.as_deref(), Some("q1"));
        assert_eq!(items[0].span_id.as_deref(), Some("01"));
    }

    #[tokio::test]
    async fn from_spans_resolves_offloaded_blob_refs_not_the_raw_reference() {
        // Regression: `Store::offload_payloads` rewrites an oversized input/output_value to an
        // `evald-blob:<key>` reference before the span is appended. `from_spans` must resolve
        // that back to the real text — otherwise the dataset holds an opaque blob id instead of
        // the content evaluators are supposed to score.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(
            dir.path(),
            crate::store::StoreConfig {
                channel_capacity: 16,
                seal_threshold_spans: 100_000,
                compact_interval: None,
                max_hot_spans: 0,
                blob_offload_bytes: 16, // tiny cap so a short string still offloads
                // Guardrail off: its production default samples the HOST's free space, and
                // a unit test must not start depending on how full the build machine is.
                disk_check_interval: None,
                disk_min_free_bytes: 0,
                disk_warn_free_bytes: 0,
                retention: None,
                redactor: None,
                ..Default::default()
            },
        )
        .unwrap();

        let long_output = "x".repeat(64);
        let mut spans = vec![span("01", Some("gpt-4o"), Some("q"), Some(&long_output))];
        let offloaded = store.offload_payloads(&mut spans);
        assert_eq!(
            offloaded, 1,
            "the oversized output_value must have been offloaded"
        );
        assert!(
            spans[0]
                .output_value
                .as_deref()
                .unwrap()
                .starts_with(crate::blob::BLOB_REF_PREFIX),
            "offload must leave a blob reference on the span: {:?}",
            spans[0].output_value
        );

        for s in &mut spans {
            resolve_blob_ref(&store, &mut s.input_value);
            resolve_blob_ref(&store, &mut s.output_value);
        }
        assert_eq!(
            spans[0].output_value.as_deref(),
            Some(long_output.as_str()),
            "resolve_blob_ref must restore the real content, not leave the blob reference"
        );
    }
}
