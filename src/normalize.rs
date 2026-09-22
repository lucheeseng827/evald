//! Normalize decoded OTLP spans into [`NormalizedSpan`], unifying the OpenInference
//! and OTel `gen_ai.*` conventions (PLAN.md §1.4, §2.1).
//!
//! Both ingest paths (protobuf in step 1, OTLP-JSON in step 2) decode into the same
//! `opentelemetry-proto` types, so normalization has a single implementation here.
//! Each LLM-semantic field is resolved by trying the convention keys in priority
//! order, so a span in either dialect — or a mix — is read correctly.

use std::collections::BTreeMap;

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value::Value as AnyVal, AnyValue, KeyValue};
use opentelemetry_proto::tonic::trace::v1::Span;
use serde_json::Value as Json;

use crate::model::{Dialect, NormalizedSpan, Tokens};

/// Flatten an export request (ResourceSpans → ScopeSpans → Span) into normalized
/// spans, threading resource (`service.name`) and scope (name/version) context down.
pub fn normalize_request(req: &ExportTraceServiceRequest) -> Vec<NormalizedSpan> {
    let mut out = Vec::new();
    // One table per request (an `Arc` clone), not per span: `crate::price` fills `cost_usd`
    // below for spans that carry a model and token counts but no cost of their own.
    let prices = crate::price::current();
    for resource_spans in &req.resource_spans {
        let service_name = resource_spans
            .resource
            .as_ref()
            .and_then(|r| attr_str(&r.attributes, "service.name"));
        // The OTel semantic-conventions version travels as a `schema_url` on the scope (or,
        // failing that, the resource) — e.g. `https://opentelemetry.io/schemas/1.27.0`. It is
        // the version "pin" for a span's conventions; capture the scope's, else the resource's.
        let schema_url = non_empty(&resource_spans.schema_url);
        for scope_spans in &resource_spans.scope_spans {
            let (scope_name, scope_version) = match &scope_spans.scope {
                Some(scope) => (non_empty(&scope.name), non_empty(&scope.version)),
                None => (None, None),
            };
            let scope_schema = non_empty(&scope_spans.schema_url).or_else(|| schema_url.clone());
            for span in &scope_spans.spans {
                let mut normalized = normalize_span(
                    span,
                    service_name.clone(),
                    scope_name.clone(),
                    scope_version.clone(),
                );
                // Surface the semconv version as a synthetic attribute (no schema/Parquet
                // column change), so it is visible/queryable without a lossy migration. Never
                // clobbers a real span attribute of the same name.
                if let Some(su) = &scope_schema {
                    normalized
                        .raw_attributes
                        .entry("otel.schema_url".to_string())
                        .or_insert_with(|| Json::String(su.clone()));
                }
                crate::price::apply(&prices, &mut normalized);
                out.push(normalized);
            }
        }
    }
    out
}

/// Normalize a single span with its resource/scope context already resolved.
pub fn normalize_span(
    span: &Span,
    service_name: Option<String>,
    scope_name: Option<String>,
    scope_version: Option<String>,
) -> NormalizedSpan {
    let a = &span.attributes;

    let mut tokens = Tokens {
        // OpenInference key first, then current gen_ai, then legacy gen_ai.
        prompt: first_u64(
            a,
            &[
                "llm.token_count.prompt",
                "gen_ai.usage.input_tokens",
                "gen_ai.usage.prompt_tokens",
            ],
        ),
        completion: first_u64(
            a,
            &[
                "llm.token_count.completion",
                "gen_ai.usage.output_tokens",
                "gen_ai.usage.completion_tokens",
            ],
        ),
        total: first_u64(a, &["llm.token_count.total", "gen_ai.usage.total_tokens"]),
        cache_read: first_u64(
            a,
            &[
                "llm.token_count.prompt_details.cache_read",
                "gen_ai.usage.cache_read.input_tokens",
            ],
        ),
        // gen_ai's cache-creation count maps to OpenInference's cache_write (PLAN.md §1.4).
        cache_write: first_u64(
            a,
            &[
                "llm.token_count.prompt_details.cache_write",
                "gen_ai.usage.cache_creation.input_tokens",
                // The current OTel GenAI spelling of the same count.
                "gen_ai.usage.cache_write.input_tokens",
            ],
        ),
        reasoning: first_u64(
            a,
            &[
                "llm.token_count.completion_details.reasoning",
                "gen_ai.usage.reasoning_tokens",
                // The current OTel GenAI spelling of the same count.
                "gen_ai.usage.reasoning.output_tokens",
            ],
        ),
    };
    // Derive total when only the parts are reported.
    if tokens.total.is_none() {
        if let (Some(p), Some(c)) = (tokens.prompt, tokens.completion) {
            tokens.total = Some(p + c);
        }
    }

    let status = span.status.as_ref();

    NormalizedSpan {
        dialect: detect_dialect(a),
        trace_id: hex::encode(&span.trace_id),
        span_id: hex::encode(&span.span_id),
        parent_span_id: if span.parent_span_id.is_empty() {
            None
        } else {
            Some(hex::encode(&span.parent_span_id))
        },
        name: span.name.clone(),
        otel_kind: span.kind,
        // Prefer the explicit OpenInference kind; otherwise infer one from gen_ai signals so an
        // OTel-native span renders as a typed span instead of "unknown".
        oi_kind: attr_str(a, "openinference.span.kind").or_else(|| infer_oi_kind(a)),
        start_unix_nano: span.start_time_unix_nano,
        end_unix_nano: span.end_time_unix_nano,
        status_code: status.map(|s| s.code).unwrap_or(0),
        status_message: status.and_then(|s| non_empty(&s.message)),
        model: first_str(
            a,
            &[
                "gen_ai.response.model",
                "gen_ai.request.model",
                "llm.model_name",
            ],
        ),
        provider: first_str(
            a,
            &[
                "gen_ai.provider.name",
                "gen_ai.system",
                "llm.provider",
                "llm.system",
            ],
        ),
        tokens,
        cost_usd: first_f64(a, &crate::price::COST_ATTRIBUTES),
        // The non-indexed keys win; only when none is present do we reconstruct from the
        // DEPRECATED indexed `gen_ai.prompt.{i}.*` / `gen_ai.completion.{i}.*` shape that
        // widely deployed SDKs (Traceloop OpenLLMetry) still emit — otherwise those spans'
        // input/output would survive only as scattered `raw_attributes`. See
        // [`indexed_messages`].
        input_value: first_str(
            a,
            &["input.value", "gen_ai.input.messages", "gen_ai.prompt"],
        )
        .or_else(|| indexed_messages(a, "prompt")),
        output_value: first_str(
            a,
            &[
                "output.value",
                "gen_ai.output.messages",
                "gen_ai.completion",
            ],
        )
        .or_else(|| indexed_messages(a, "completion")),
        session_id: first_str(a, &["session.id", "gen_ai.conversation.id"]),
        user_id: first_str(a, &["user.id"]),
        service_name,
        scope_name,
        scope_version,
        raw_attributes: raw_attributes(a),
    }
}

/// Infer an OpenInference-style span kind for OTel-native / `gen_ai.*` spans that don't carry
/// an explicit `openinference.span.kind`, so they render as a typed span (LLM / EMBEDDING /
/// TOOL / AGENT) instead of "unknown" — many trace viewers type only `llm.*` spans and leave
/// the rest unclassified. Returns `None` when there is no gen_ai signal, so a genuinely non-model span
/// stays unclassified rather than being mislabeled.
fn infer_oi_kind(attrs: &[KeyValue]) -> Option<String> {
    // The OTel `gen_ai.operation.name` is the most direct signal.
    if let Some(op) = attr_str(attrs, "gen_ai.operation.name") {
        let kind = match op.to_ascii_lowercase().as_str() {
            "embeddings" | "embedding" => "EMBEDDING",
            "execute_tool" | "tool" => "TOOL",
            "create_agent" | "invoke_agent" | "agent" => "AGENT",
            // chat / text_completion / generate_content / … and any other named gen_ai
            // operation is a model call.
            _ => "LLM",
        };
        return Some(kind.to_string());
    }
    // No operation name, but gen_ai request/response/usage attributes ⇒ a model call.
    let has_genai_call = attrs.iter().any(|kv| {
        kv.key.starts_with("gen_ai.request.")
            || kv.key.starts_with("gen_ai.response.")
            || kv.key.starts_with("gen_ai.usage.")
    });
    if has_genai_call {
        return Some("LLM".to_string());
    }
    // gen_ai tool attributes without an operation name ⇒ a tool span.
    if attrs.iter().any(|kv| kv.key.starts_with("gen_ai.tool.")) {
        return Some("TOOL".to_string());
    }
    None
}

/// Classify a span's convention from its attribute keys (informational only).
fn detect_dialect(attrs: &[KeyValue]) -> Dialect {
    let has_oi = attrs.iter().any(|kv| {
        kv.key == "openinference.span.kind"
            || kv.key.starts_with("llm.")
            || kv.key.starts_with("openinference.")
    });
    let has_genai = attrs.iter().any(|kv| kv.key.starts_with("gen_ai."));
    match (has_oi, has_genai) {
        (true, true) => Dialect::Mixed,
        (true, false) => Dialect::OpenInference,
        (false, true) => Dialect::GenAi,
        (false, false) => Dialect::Unknown,
    }
}

/// Lossless attribute copy: key → JSON value, sorted.
fn raw_attributes(attrs: &[KeyValue]) -> BTreeMap<String, Json> {
    attrs
        .iter()
        .map(|kv| {
            let json = kv
                .value
                .as_ref()
                .map(anyvalue_to_json)
                .unwrap_or(Json::Null);
            (kv.key.clone(), json)
        })
        .collect()
}

/// Convert an OTLP `AnyValue` to a `serde_json::Value`, recursing through arrays and
/// kv-lists. Bytes are hex-encoded; the profiling-only string-table index is dropped.
fn anyvalue_to_json(v: &AnyValue) -> Json {
    match &v.value {
        None => Json::Null,
        Some(AnyVal::StringValue(s)) => Json::String(s.clone()),
        Some(AnyVal::BoolValue(b)) => Json::Bool(*b),
        Some(AnyVal::IntValue(i)) => Json::Number((*i).into()),
        Some(AnyVal::DoubleValue(d)) => serde_json::Number::from_f64(*d)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Some(AnyVal::ArrayValue(arr)) => {
            Json::Array(arr.values.iter().map(anyvalue_to_json).collect())
        }
        Some(AnyVal::KvlistValue(kv)) => Json::Object(
            kv.values
                .iter()
                .map(|item| {
                    let val = item
                        .value
                        .as_ref()
                        .map(anyvalue_to_json)
                        .unwrap_or(Json::Null);
                    (item.key.clone(), val)
                })
                .collect(),
        ),
        Some(AnyVal::BytesValue(b)) => Json::String(hex::encode(b)),
        Some(AnyVal::StringValueStrindex(_)) => Json::Null,
    }
}

// --- attribute lookup helpers ----------------------------------------------------

fn attr<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a AnyValue> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
}

/// Coerce an `AnyValue` to a scalar string: a string as-is; int/double/bool stringified.
///
/// Tolerant ingest — a producer that sends a normally-string field as a scalar (loose typing /
/// semconv drift, e.g. a stringly-typed model id, or `gen_ai.request.seed` arriving as an int on
/// one SDK version and a string on the next) still resolves rather than being silently dropped.
/// Arrays / kvlists / bytes have no sensible scalar form, so they don't coerce (`None`).
fn scalar_string(v: &AnyValue) -> Option<String> {
    match v.value.as_ref()? {
        AnyVal::StringValue(s) => Some(s.clone()),
        AnyVal::IntValue(i) => Some(i.to_string()),
        AnyVal::DoubleValue(d) => Some(d.to_string()),
        AnyVal::BoolValue(b) => Some(b.to_string()),
        _ => None,
    }
}

pub(crate) fn attr_str(attrs: &[KeyValue], key: &str) -> Option<String> {
    attr(attrs, key).and_then(scalar_string)
}

/// Read an attribute as a non-negative integer. Accepts int, integral double, or a
/// numeric string (so it is robust to producers that stringify counts).
fn attr_u64(attrs: &[KeyValue], key: &str) -> Option<u64> {
    match attr(attrs, key)?.value.as_ref()? {
        AnyVal::IntValue(i) if *i >= 0 => Some(*i as u64),
        AnyVal::DoubleValue(d) if *d >= 0.0 && d.fract() == 0.0 => Some(*d as u64),
        AnyVal::StringValue(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

pub(crate) fn attr_f64(attrs: &[KeyValue], key: &str) -> Option<f64> {
    match attr(attrs, key)?.value.as_ref()? {
        AnyVal::DoubleValue(d) => Some(*d),
        AnyVal::IntValue(i) => Some(*i as f64),
        AnyVal::StringValue(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn first_str(attrs: &[KeyValue], keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| attr_str(attrs, k))
}

fn first_u64(attrs: &[KeyValue], keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| attr_u64(attrs, k))
}

fn first_f64(attrs: &[KeyValue], keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| attr_f64(attrs, k))
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Cap on the messages [`indexed_messages`] will reconstruct from one span. The receiver
/// ingests untrusted OTLP, and these per-message attributes come off the wire, so bound the
/// scan — far above any real chat history.
const MAX_INDEXED_MESSAGES: usize = 4096;

/// Reconstruct a messages array from the **deprecated indexed** `gen_ai` shape —
/// `gen_ai.<kind>.{i}.role` / `gen_ai.<kind>.{i}.content` (`kind` = `"prompt"` for input,
/// `"completion"` for output) — into a JSON array `[{"role":…,"content":…}, …]`.
///
/// This flat message shape is the same one the bare `gen_ai.prompt` / `gen_ai.completion`
/// string and the structured `gen_ai.{input,output}.messages` attributes carry, so a span
/// instrumented with the legacy indexed shape lands the *same* normalized input/output as a
/// modern one. The indexed shape was deprecated in the OTel GenAI semconv (v1.38.0) but is
/// still emitted by widely deployed instrumentation (Traceloop OpenLLMetry), so deployed SDKs
/// will produce it for a long time; without this those prompts/completions would appear only
/// as scattered `raw_attributes`, invisible to the read/UI/cost/eval-curation surfaces that
/// read `input_value` / `output_value`.
///
/// Fallback-only (the caller tries the non-indexed keys first). Collects this `kind`'s indexed
/// attributes in ONE pass over `attrs` (not a per-index rescan), keyed by message index, then
/// emits contiguous indices from `0`, stopping at the first **absent** index — how SDKs emit
/// them — bounded by [`MAX_INDEXED_MESSAGES`]. A message may be role-only or content-only.
///
/// Crucially, an index counts as *present* when any of its sub-keys exists, independent of
/// whether the value coerces to a scalar: a slot whose `…content` is a non-scalar (a structured
/// tool-call value) yields no scalar message field but does **not** terminate the scan, so later
/// messages are never silently dropped. Non-scalar values remain in `raw_attributes` (copied
/// there wholesale); they simply aren't flattened into a message here.
fn indexed_messages(attrs: &[KeyValue], kind: &str) -> Option<String> {
    // One pass: bucket `gen_ai.<kind>.<i>.{role,content}` into per-index scalar parts. The map
    // key is the index, so iteration order below is ascending; inserting an entry (even for an
    // unhandled sub-key like `…tool_calls.*`) marks that index present.
    let prefix = format!("gen_ai.{kind}.");
    let mut by_index: BTreeMap<usize, (Option<String>, Option<String>)> = BTreeMap::new();
    for kv in attrs {
        let Some(rest) = kv.key.strip_prefix(&prefix) else {
            continue;
        };
        let Some((idx_str, field)) = rest.split_once('.') else {
            continue;
        };
        let Ok(idx) = idx_str.parse::<usize>() else {
            continue;
        };
        if idx >= MAX_INDEXED_MESSAGES {
            continue;
        }
        let entry = by_index.entry(idx).or_default();
        match field {
            "role" => entry.0 = kv.value.as_ref().and_then(scalar_string),
            "content" => entry.1 = kv.value.as_ref().and_then(scalar_string),
            _ => {} // other sub-keys only mark the index present (kept in raw_attributes)
        }
    }

    let mut messages: Vec<Json> = Vec::new();
    let mut i = 0usize;
    // Contiguous from 0; a gap (fully-absent index) ends the message list. A present slot with
    // no scalar role/content is skipped but does not stop the scan.
    while let Some((role, content)) = by_index.get(&i) {
        i += 1;
        if role.is_none() && content.is_none() {
            continue;
        }
        let mut msg = serde_json::Map::new();
        if let Some(r) = role {
            msg.insert("role".to_string(), Json::String(r.clone()));
        }
        if let Some(c) = content {
            msg.insert("content".to_string(), Json::String(c.clone()));
        }
        messages.push(Json::Object(msg));
    }
    if messages.is_empty() {
        return None;
    }
    // Serializing owned JSON never fails; fall back to None rather than panic.
    serde_json::to_string(&Json::Array(messages)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::trace::v1::Span;

    fn kv(key: &str, value: AnyVal) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
            ..Default::default()
        }
    }
    fn s(key: &str, v: &str) -> KeyValue {
        kv(key, AnyVal::StringValue(v.to_string()))
    }
    fn i(key: &str, v: i64) -> KeyValue {
        kv(key, AnyVal::IntValue(v))
    }

    fn span_with(attrs: Vec<KeyValue>) -> Span {
        Span {
            trace_id: vec![0x01; 16],
            span_id: vec![0x02; 8],
            name: "llm.call".to_string(),
            kind: 3,
            start_time_unix_nano: 100,
            end_time_unix_nano: 350,
            attributes: attrs,
            ..Default::default()
        }
    }

    fn norm(attrs: Vec<KeyValue>) -> NormalizedSpan {
        normalize_span(&span_with(attrs), Some("svc".into()), None, None)
    }

    #[test]
    fn gen_ai_operation_infers_span_kind() {
        // OTel-native gen_ai spans (no `openinference.span.kind`) get a kind inferred, so they
        // render typed instead of "unknown".
        let kind = |attrs| norm(attrs).oi_kind;
        assert_eq!(
            kind(vec![s("gen_ai.operation.name", "chat")]).as_deref(),
            Some("LLM")
        );
        assert_eq!(
            kind(vec![s("gen_ai.operation.name", "embeddings")]).as_deref(),
            Some("EMBEDDING")
        );
        assert_eq!(
            kind(vec![s("gen_ai.operation.name", "execute_tool")]).as_deref(),
            Some("TOOL")
        );
        assert_eq!(
            kind(vec![s("gen_ai.operation.name", "invoke_agent")]).as_deref(),
            Some("AGENT")
        );
        // No operation name, but a gen_ai request attribute is still a model call.
        assert_eq!(
            kind(vec![s("gen_ai.request.model", "gpt-4o")]).as_deref(),
            Some("LLM")
        );
        // A genuinely non-model span stays unclassified (not mislabeled).
        assert_eq!(kind(vec![s("http.request.method", "GET")]), None);
    }

    #[test]
    fn explicit_oi_kind_wins_over_inference() {
        let n = norm(vec![
            s("openinference.span.kind", "RETRIEVER"),
            s("gen_ai.operation.name", "chat"), // would infer LLM, but the explicit kind wins
        ]);
        assert_eq!(n.oi_kind.as_deref(), Some("RETRIEVER"));
    }

    #[test]
    fn schema_url_captured_as_synthetic_attribute() {
        use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
        use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
        use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans};

        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: None,
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "openinference".into(),
                        version: "0.1".into(),
                        ..Default::default()
                    }),
                    spans: vec![span_with(vec![s("gen_ai.request.model", "gpt-4o")])],
                    schema_url: "https://opentelemetry.io/schemas/1.27.0".into(),
                }],
                schema_url: String::new(),
            }],
        };
        let spans = normalize_request(&req);
        assert_eq!(spans.len(), 1);
        // The semconv version (schema_url) is surfaced as a synthetic attribute…
        assert_eq!(
            spans[0]
                .raw_attributes
                .get("otel.schema_url")
                .and_then(|v| v.as_str()),
            Some("https://opentelemetry.io/schemas/1.27.0")
        );
        // …and the OTel-native span-kind inference also fired.
        assert_eq!(spans[0].oi_kind.as_deref(), Some("LLM"));
    }

    #[test]
    fn openinference_span_normalizes() {
        let n = norm(vec![
            s("openinference.span.kind", "LLM"),
            s("llm.model_name", "claude-opus-4-8"),
            s("llm.provider", "anthropic"),
            i("llm.token_count.prompt", 1200),
            i("llm.token_count.completion", 300),
            i("llm.token_count.prompt_details.cache_read", 1000),
            s("input.value", "hello?"),
            s("output.value", "hi!"),
            s("session.id", "sess-1"),
        ]);
        assert_eq!(n.dialect, Dialect::OpenInference);
        assert_eq!(n.oi_kind.as_deref(), Some("LLM"));
        assert_eq!(n.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(n.provider.as_deref(), Some("anthropic"));
        assert_eq!(n.tokens.prompt, Some(1200));
        assert_eq!(n.tokens.completion, Some(300));
        assert_eq!(n.tokens.total, Some(1500)); // derived
        assert_eq!(n.tokens.cache_read, Some(1000));
        assert_eq!(n.input_value.as_deref(), Some("hello?"));
        assert_eq!(n.output_value.as_deref(), Some("hi!"));
        assert_eq!(n.session_id.as_deref(), Some("sess-1"));
        assert_eq!(n.duration_ns(), 250);
    }

    #[test]
    fn gen_ai_span_normalizes_to_same_fields() {
        let n = norm(vec![
            s("gen_ai.provider.name", "openai"),
            s("gen_ai.request.model", "gpt-4o"),
            s("gen_ai.response.model", "gpt-4o-2024-08-06"),
            i("gen_ai.usage.input_tokens", 800),
            i("gen_ai.usage.output_tokens", 120),
            i("gen_ai.usage.cache_creation.input_tokens", 50),
        ]);
        assert_eq!(n.dialect, Dialect::GenAi);
        // response model wins over request model
        assert_eq!(n.model.as_deref(), Some("gpt-4o-2024-08-06"));
        assert_eq!(n.provider.as_deref(), Some("openai"));
        assert_eq!(n.tokens.prompt, Some(800));
        assert_eq!(n.tokens.completion, Some(120));
        assert_eq!(n.tokens.total, Some(920));
        // gen_ai cache-creation maps to cache_write
        assert_eq!(n.tokens.cache_write, Some(50));
    }

    #[test]
    fn legacy_gen_ai_token_names_still_map() {
        let n = norm(vec![
            i("gen_ai.usage.prompt_tokens", 10),
            i("gen_ai.usage.completion_tokens", 5),
        ]);
        assert_eq!(n.tokens.prompt, Some(10));
        assert_eq!(n.tokens.completion, Some(5));
    }

    #[test]
    fn deprecated_gen_ai_system_is_used_as_provider() {
        let n = norm(vec![s("gen_ai.system", "anthropic")]);
        assert_eq!(n.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn indexed_gen_ai_messages_are_reconstructed() {
        // The deprecated indexed shape OpenLLMetry still emits — evald reconstructs the same
        // messages array the modern shapes carry, instead of dropping the prompt/completion.
        let n = norm(vec![
            s("gen_ai.request.model", "gpt-4o"),
            s("gen_ai.prompt.0.role", "system"),
            s("gen_ai.prompt.0.content", "You are helpful."),
            s("gen_ai.prompt.1.role", "user"),
            s("gen_ai.prompt.1.content", "Hi"),
            s("gen_ai.completion.0.role", "assistant"),
            s("gen_ai.completion.0.content", "Hello!"),
        ]);
        let input: Json = serde_json::from_str(n.input_value.as_deref().unwrap()).unwrap();
        assert_eq!(
            input,
            serde_json::json!([
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hi"},
            ])
        );
        let output: Json = serde_json::from_str(n.output_value.as_deref().unwrap()).unwrap();
        assert_eq!(
            output,
            serde_json::json!([{"role": "assistant", "content": "Hello!"}])
        );
        // The raw indexed attributes are still preserved losslessly alongside.
        assert_eq!(
            n.raw_attributes
                .get("gen_ai.prompt.0.content")
                .and_then(|v| v.as_str()),
            Some("You are helpful.")
        );
    }

    #[test]
    fn non_indexed_shapes_win_over_indexed() {
        // A bare `gen_ai.prompt` string takes precedence over indexed parts (existing behavior
        // preserved; the fallback only fills a gap, it never double-sources).
        let n = norm(vec![
            s("gen_ai.prompt", "bare prompt"),
            s("gen_ai.prompt.0.content", "indexed prompt"),
        ]);
        assert_eq!(n.input_value.as_deref(), Some("bare prompt"));

        // Structured `gen_ai.output.messages` likewise wins over indexed completion parts.
        let n = norm(vec![
            s(
                "gen_ai.output.messages",
                r#"[{"role":"assistant","content":"structured"}]"#,
            ),
            s("gen_ai.completion.0.content", "indexed"),
        ]);
        assert_eq!(
            n.output_value.as_deref(),
            Some(r#"[{"role":"assistant","content":"structured"}]"#)
        );
    }

    #[test]
    fn indexed_messages_handle_partial_and_absent() {
        // A content-only message (no role) is still captured.
        let n = norm(vec![s("gen_ai.prompt.0.content", "just content")]);
        let input: Json = serde_json::from_str(n.input_value.as_deref().unwrap()).unwrap();
        assert_eq!(input, serde_json::json!([{"content": "just content"}]));

        // Non-contiguous indices stop at the first fully-absent slot: index 0 present, index 1
        // absent, index 2 present → only message 0 is reconstructed (matches how SDKs emit).
        let n = norm(vec![
            s("gen_ai.prompt.0.content", "first"),
            s("gen_ai.prompt.2.content", "third"),
        ]);
        let input: Json = serde_json::from_str(n.input_value.as_deref().unwrap()).unwrap();
        assert_eq!(input, serde_json::json!([{"content": "first"}]));

        // A span with no message attributes reconstructs nothing.
        let n = norm(vec![s("gen_ai.request.model", "gpt-4o")]);
        assert_eq!(n.input_value, None);
        assert_eq!(n.output_value, None);
    }

    #[test]
    fn indexed_non_scalar_content_does_not_truncate_later_messages() {
        use opentelemetry_proto::tonic::common::v1::ArrayValue;
        // index 0: structured (array) content, no role → neither field coerces to a scalar, but
        // the key IS present. This must NOT stop the contiguous scan; index 1 (scalar) is still
        // emitted. A presence-blind `break` (the pre-fix behavior) would have lost it entirely.
        let structured = KeyValue {
            key: "gen_ai.prompt.0.content".to_string(),
            value: Some(AnyValue {
                value: Some(AnyVal::ArrayValue(ArrayValue {
                    values: vec![AnyValue {
                        value: Some(AnyVal::StringValue("part".to_string())),
                    }],
                })),
            }),
            ..Default::default()
        };
        let n = norm(vec![
            structured,
            s("gen_ai.prompt.1.role", "user"),
            s("gen_ai.prompt.1.content", "hello"),
        ]);
        let input: Json = serde_json::from_str(n.input_value.as_deref().unwrap()).unwrap();
        assert_eq!(
            input,
            serde_json::json!([{"role": "user", "content": "hello"}])
        );
        // …and the non-scalar value is still preserved losslessly in raw_attributes.
        assert!(n
            .raw_attributes
            .get("gen_ai.prompt.0.content")
            .unwrap()
            .is_array());
    }

    #[test]
    fn both_conventions_present_is_mixed() {
        let n = norm(vec![
            s("openinference.span.kind", "LLM"),
            s("gen_ai.request.model", "x"),
        ]);
        assert_eq!(n.dialect, Dialect::Mixed);
    }

    #[test]
    fn non_llm_span_is_unknown_but_still_normalized() {
        let n = norm(vec![s("http.request.method", "GET")]);
        assert_eq!(n.dialect, Dialect::Unknown);
        assert_eq!(n.tokens, Tokens::default());
        assert_eq!(n.raw_attributes.len(), 1);
    }

    #[test]
    fn raw_attributes_are_lossless_and_typed() {
        let n = norm(vec![
            s("str", "v"),
            i("int", 7),
            kv("flag", AnyVal::BoolValue(true)),
            kv("dbl", AnyVal::DoubleValue(1.5)),
        ]);
        assert_eq!(n.raw_attributes["str"], Json::String("v".into()));
        assert_eq!(n.raw_attributes["int"], Json::from(7));
        assert_eq!(n.raw_attributes["flag"], Json::Bool(true));
        assert_eq!(n.raw_attributes["dbl"], Json::from(1.5));
    }

    #[test]
    fn numeric_string_token_counts_are_tolerated() {
        // A producer that stringifies counts inside the proto value (not just OTLP-JSON).
        let n = norm(vec![s("gen_ai.usage.input_tokens", "640")]);
        assert_eq!(n.tokens.prompt, Some(640));
    }

    #[test]
    fn string_fields_tolerate_loose_scalar_types() {
        // Semconv drift / loose typing: a string-typed field arriving as a non-string scalar still
        // resolves (coerced) instead of being silently dropped. Locks the tolerant-ingest contract.
        let n = norm(vec![
            i("llm.model_name", 42),                     // int where a string is expected
            kv("llm.provider", AnyVal::BoolValue(true)), // bool
            kv("session.id", AnyVal::DoubleValue(7.0)),  // double
        ]);
        assert_eq!(n.model.as_deref(), Some("42"));
        assert_eq!(n.provider.as_deref(), Some("true"));
        assert_eq!(n.session_id.as_deref(), Some("7"));
    }

    #[test]
    fn semconv_drift_across_revisions_all_normalize() {
        // One matrix locking the cross-revision tolerance the normalizer already provides: the
        // deprecated `gen_ai.system` provider key, legacy `*_tokens` names, and current gen_ai keys.
        let legacy = norm(vec![
            s("gen_ai.system", "anthropic"),
            i("gen_ai.usage.prompt_tokens", 11),
            i("gen_ai.usage.completion_tokens", 5),
        ]);
        assert_eq!(legacy.provider.as_deref(), Some("anthropic"));
        assert_eq!(legacy.tokens.prompt, Some(11));

        let current = norm(vec![
            s("gen_ai.provider.name", "anthropic"),
            i("gen_ai.usage.input_tokens", 11),
            i("gen_ai.usage.output_tokens", 5),
        ]);
        assert_eq!(current.provider.as_deref(), Some("anthropic"));
        assert_eq!(current.tokens.prompt, Some(11));
        // Both revisions land on the same normalized shape.
        assert_eq!(legacy.provider, current.provider);
        assert_eq!(legacy.tokens.prompt, current.tokens.prompt);
    }
}
