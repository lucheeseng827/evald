//! Emit a tiny OTLP/HTTP-protobuf `ExportTraceServiceRequest` to stdout.
//!
//! A dependency-free fixture for manually exercising the receiver (PoC step 1) before
//! a real OpenInference/OTel SDK is wired in. Pipe it straight to the running server:
//!
//! ```text
//! evald serve &
//! cargo run --example otlp_demo_payload \
//!   | curl --data-binary @- -H 'Content-Type: application/x-protobuf' \
//!          http://127.0.0.1:4318/v1/traces
//! ```
//!
//! The payload is one resource (`service.name=demo-app`) → one scope → one LLM span
//! carrying OpenInference (`openinference.span.kind`) and gen_ai (`gen_ai.request.model`)
//! attributes, so the server log shows it parsing both conventions.

use std::io::Write;

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value::Value as AnyVal, AnyValue, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;

fn str_kv(key: &str, val: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AnyVal::StringValue(val.to_string())),
        }),
        ..Default::default()
    }
}

fn int_kv(key: &str, val: i64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AnyVal::IntValue(val)),
        }),
        ..Default::default()
    }
}

fn main() -> std::io::Result<()> {
    let span = Span {
        trace_id: vec![0xab; 16],
        span_id: vec![0xcd; 8],
        name: "openai.chat.completions".to_string(),
        kind: 3, // CLIENT
        start_time_unix_nano: 1_700_000_000_000_000_000,
        end_time_unix_nano: 1_700_000_000_842_000_000,
        attributes: vec![
            str_kv("openinference.span.kind", "LLM"),
            str_kv("gen_ai.request.model", "claude-opus-4-8"),
            str_kv("gen_ai.system", "anthropic"),
            int_kv("gen_ai.usage.input_tokens", 1875),
            int_kv("gen_ai.usage.output_tokens", 432),
        ],
        ..Default::default()
    };
    let request = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![str_kv("service.name", "demo-app")],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: vec![span],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    std::io::stdout().write_all(&request.encode_to_vec())
}
