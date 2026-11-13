//! vLLM's own request spans, read as the server side of a model call.
//!
//! Started with `--otlp-traces-endpoint`, vLLM exports one span per request: `llm_request`,
//! kind `SERVER`, starting when the request arrived. Its attributes are the server's view of
//! the call: prompt and completion tokens, and where the time went, in seconds
//! (`gen_ai.latency.time_in_queue`, `…time_in_model_prefill`, `…time_in_model_decode`,
//! `…time_in_model_inference`, `…time_to_first_token`, `…e2e`). It names no model. Read from
//! `vllm/v1/engine/output_processor.py` (`do_tracing`) and `vllm/tracing/utils.py` at vLLM
//! f46ea25e (2026-10-22). It exports over OTLP/gRPC by default, or OTLP/HTTP with
//! `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf`; evald takes both.
//!
//! When the app propagates `traceparent` through its OpenAI-compatible call, vLLM's span is a
//! child of the app's own model-call span: the same call, seen from both ends. The app's span
//! is the record of the call (it names the model, and is what the app paid for); the server's
//! usage is a second copy of it, so it is set aside like a coding agent's traced copy
//! ([`crate::agent::USAGE_COPY`]), and the span keeps its timings. Without a parent, the
//! server span is the only record of the call, and counts as one.
//!
//! Either way the span is marked `evald.server = "vllm"`, and `evald latency` reports the
//! server's queue, prefill and decode time beside the client's view ([`crate::latency`]).

use serde_json::Value as Json;

use crate::model::NormalizedSpan;

/// The name vLLM gives its request span.
pub const SERVER_SPAN_NAME: &str = "llm_request";
/// Marks a span an inference server emitted about a request it served.
pub const SERVER_KEY: &str = "evald.server";
pub const VLLM: &str = "vllm";
/// The OTLP `SpanKind` for a server span.
const SPAN_KIND_SERVER: i32 = 2;
/// The end-to-end time vLLM measured (seconds), present on every request span it emits.
pub const E2E: &str = "gen_ai.latency.e2e";

/// vLLM's timings, in the order a request spends them (seconds).
pub const PHASES: [(&str, &str); 4] = [
    ("queue", "gen_ai.latency.time_in_queue"),
    ("prefill", "gen_ai.latency.time_in_model_prefill"),
    ("decode", "gen_ai.latency.time_in_model_decode"),
    ("server", E2E),
];

/// Whether `s` is vLLM's span for a request it served.
pub fn is_server_span(s: &NormalizedSpan) -> bool {
    s.name == SERVER_SPAN_NAME
        && s.otel_kind == SPAN_KIND_SERVER
        && s.raw_attributes.contains_key(E2E)
}

/// Ingest-time pass (from [`crate::normalize::normalize_span`]): mark vLLM's request span, and
/// set its usage aside when the app's own span of the call is its parent.
pub(crate) fn normalize(s: &mut NormalizedSpan) {
    if !is_server_span(s) {
        return;
    }
    s.raw_attributes
        .insert(SERVER_KEY.into(), Json::String(VLLM.into()));
    // A served model call, whatever kind was inferred from its attributes.
    s.oi_kind = Some("LLM".into());
    if s.parent_span_id.is_some() {
        crate::agent::set_aside(s, crate::agent::USAGE_COPY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::normalize_span;
    use opentelemetry_proto::tonic::common::v1::{any_value::Value, AnyValue, KeyValue};
    use opentelemetry_proto::tonic::trace::v1::Span;

    fn kv(k: &str, v: Value) -> KeyValue {
        KeyValue {
            key: k.into(),
            value: Some(AnyValue { value: Some(v) }),
            ..Default::default()
        }
    }

    /// vLLM's span as `do_tracing` builds it.
    fn vllm_span(parent: Option<[u8; 8]>) -> Span {
        Span {
            trace_id: vec![7; 16],
            span_id: vec![9; 8],
            parent_span_id: parent.map(|p| p.to_vec()).unwrap_or_default(),
            name: SERVER_SPAN_NAME.into(),
            kind: SPAN_KIND_SERVER,
            start_time_unix_nano: 1_000_000_000,
            end_time_unix_nano: 1_900_000_000,
            attributes: vec![
                kv(
                    "gen_ai.latency.time_to_first_token",
                    Value::DoubleValue(0.31),
                ),
                kv(E2E, Value::DoubleValue(0.9)),
                kv("gen_ai.latency.time_in_queue", Value::DoubleValue(0.12)),
                kv("gen_ai.usage.prompt_tokens", Value::IntValue(512)),
                kv("gen_ai.usage.completion_tokens", Value::IntValue(64)),
                kv(
                    "gen_ai.latency.time_in_model_prefill",
                    Value::DoubleValue(0.19),
                ),
                kv(
                    "gen_ai.latency.time_in_model_decode",
                    Value::DoubleValue(0.59),
                ),
                kv(
                    "gen_ai.latency.time_in_model_inference",
                    Value::DoubleValue(0.78),
                ),
                kv("gen_ai.request.id", Value::StringValue("cmpl-1".into())),
                kv("gen_ai.request.max_tokens", Value::IntValue(256)),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn an_unparented_server_span_is_the_call() {
        let s = normalize_span(&vllm_span(None), None, None, None);
        assert_eq!(s.raw_attributes[SERVER_KEY], VLLM);
        assert!(s.is_llm_span());
        assert_eq!(s.tokens.prompt, Some(512));
        assert_eq!(s.tokens.completion, Some(64));
        assert!(!s.raw_attributes.contains_key(crate::agent::USAGE_KEY));
    }

    #[test]
    fn a_parented_server_span_is_a_copy_and_keeps_its_timings() {
        let s = normalize_span(&vllm_span(Some([3; 8])), None, None, None);
        assert_eq!(s.raw_attributes[SERVER_KEY], VLLM);
        assert!(!s.is_llm_span(), "the app's span is the model call");
        assert!(s.tokens.is_empty() && s.cost_usd.is_none());
        assert_eq!(
            s.raw_attributes[crate::agent::USAGE_KEY],
            crate::agent::USAGE_COPY
        );
        // The usage it reported and its timings stay queryable.
        assert_eq!(s.raw_attributes["gen_ai.usage.prompt_tokens"], 512);
        assert_eq!(s.raw_attributes["gen_ai.latency.time_in_queue"], 0.12);
    }

    #[test]
    fn other_spans_named_llm_request_are_left_alone() {
        let mut client = vllm_span(None);
        client.kind = 3; // CLIENT
        let s = normalize_span(&client, None, None, None);
        assert!(!s.raw_attributes.contains_key(SERVER_KEY));
        let mut no_timing = vllm_span(None);
        no_timing.attributes.retain(|a| a.key != E2E);
        assert!(!normalize_span(&no_timing, None, None, None)
            .raw_attributes
            .contains_key(SERVER_KEY));
    }
}
