//! Claude Code's telemetry, mapped (0.4.0, `ROADMAP-0.4-0.6.md` 4.2).
//!
//! Claude Code exports **events** over the OTLP logs signal (stable, the useful default) and,
//! behind `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`, **spans** over traces. The mapping here was
//! written against what version 2.1.306 actually sends (`tests/fixtures/claude-code/2.1.306/`,
//! captured with `scripts/otlp-fixture.py`), not against its documentation; the docs and the
//! wire disagree on details that matter, such as `duration_ms` arriving as a string on tool
//! events and an integer on API events.
//!
//! # Events → spans
//!
//! | Event | Becomes |
//! |---|---|
//! | `api_request` | an LLM span `claude_code.llm_request`: model, tokens, Claude Code's own `cost_usd`, duration, time to first token |
//! | `api_error` | the same span shape with an error status and the error message |
//! | `tool_result` | a TOOL span `claude_code.tool`: tool name, call id, duration, an error status when it failed |
//! | `tool_decision` with `decision=reject` | a TOOL span with an error status `rejected (<source>)`: a refused tool has no result event |
//! | `user_prompt` | a zero-duration `claude_code.user_prompt` marker, one per turn |
//! | `tool_decision` accept, `assistant_response`, `api_refusal` | folded: the result / request event carries the facts |
//!
//! Every other event (startup, plugins, hooks, settings) is not a GenAI fact and is left to the
//! logs policy. All of a record's own attributes are kept on its span, as they are for a
//! received span, plus their `gen_ai.*` spellings so tokens, cost and kind are read by the one
//! normalisation every span gets.
//!
//! **Tokens.** Claude Code reports `input_tokens` *excluding* cache reads and writes (as
//! Anthropic's API does). The mapping states the total explicitly, which is what lets
//! `crate::price` see the cache counts as additive and price each token once.
//!
//! # Where a span sits
//!
//! A record carries a trace context when Claude Code is tracing (the interaction span), or when
//! its environment had a `TRACEPARENT` (`claude -p`, the Agent SDK). A span made from it uses
//! that trace and parents to that span. A record with none gets a trace derived from its
//! `session.id` and `prompt.id`, and parents to its turn's `user_prompt` marker, so one turn is
//! one trace tree either way.
//!
//! # One call, one span, when both signals are on
//!
//! With tracing enabled, every API call is described twice: an `api_request` event and a
//! `claude_code.llm_request` span. Both name the call by the server's `request_id`, so both are
//! given the span id derived from it ([`llm_span_id`]) and the same call key: the store keeps
//! whichever arrives first and drops the other at ingest ([`crate::calls`]), so cost, tokens
//! and `/metrics` count the call once. The event copy carries Claude Code's own cost; a span
//! copy that wins is priced from the price table. The span's original id is kept as
//! `evald.otel.span_id`. It is safe to replace because an LLM request span is a leaf.
//!
//! Tool spans are not leaves (permission wait, execution and subagent spans hang under them),
//! so a traced `claude_code.tool` span keeps its id. It still anchors the call: a `tool_result`
//! event arriving after it is dropped. One arriving before it is kept, so with both signals on
//! a tool call can appear twice in raw SQL (`evald.source = 'log'` marks the event copy); the
//! session report counts distinct `gen_ai.tool.call.id`.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use serde_json::Value as Json;

use crate::calls::{CALL_ANCHOR, CALL_KEY};
use crate::logs::{
    derive_id, kv_f64, kv_int, kv_str, record_time, trace_context, Mapping, RecordCtx, SpanDraft,
};
use crate::model::NormalizedSpan;
use crate::normalize::{attr_f64, attr_str, attr_u64};

/// The instrumentation scope of Claude Code's events.
pub const EVENTS_SCOPE: &str = "com.anthropic.claude_code.events";
/// The instrumentation scope of Claude Code's spans.
pub const TRACING_SCOPE: &str = "com.anthropic.claude_code.tracing";
/// The `service.name` Claude Code sets.
pub const SERVICE_NAME: &str = "claude-code";

const LLM_SPAN: &str = "claude_code.llm_request";
const TOOL_SPAN: &str = "claude_code.tool";
const PROMPT_SPAN: &str = "claude_code.user_prompt";

/// Is this record Claude Code's?
fn is_mine(ctx: &RecordCtx) -> bool {
    ctx.scope_name
        .is_some_and(|s| s.starts_with("com.anthropic.claude_code"))
        || ctx.service_name.as_deref() == Some(SERVICE_NAME)
}

/// Map one Claude Code event record ([`crate::logs`] calls this for every record).
pub(crate) fn map_log(ctx: &RecordCtx, rec: &LogRecord, name: Option<&str>) -> Mapping {
    if !is_mine(ctx) {
        return Mapping::NotMine;
    }
    let Some(name) = name else {
        return Mapping::NotMine;
    };
    // The body carries `claude_code.api_request`, the `event.name` attribute `api_request`.
    let event = name.strip_prefix("claude_code.").unwrap_or(name);
    let a = &rec.attributes;
    match event {
        "api_request" => Mapping::Span(Box::new(llm_span(ctx, rec, None))),
        "api_error" => {
            let msg = attr_str(a, "error").unwrap_or_else(|| "API request failed".into());
            Mapping::Span(Box::new(llm_span(ctx, rec, Some(msg))))
        }
        "tool_result" => Mapping::Span(Box::new(tool_span(ctx, rec, None))),
        "tool_decision" => {
            if attr_str(a, "decision").as_deref() == Some("reject") {
                let source = attr_str(a, "source").unwrap_or_else(|| "unknown".into());
                Mapping::Span(Box::new(tool_span(
                    ctx,
                    rec,
                    Some(format!("rejected ({source})")),
                )))
            } else {
                Mapping::Folded
            }
        }
        "user_prompt" => Mapping::Span(Box::new(prompt_span(ctx, rec))),
        "assistant_response" | "api_refusal" => Mapping::Folded,
        _ => Mapping::NotMine,
    }
}

/// The span id both signals give one API call: derived from the server's request id.
pub fn llm_span_id(request_id: &str) -> Vec<u8> {
    derive_id(8, &["claude_code.llm", request_id])
}

fn tool_span_id(tool_use_id: &str) -> Vec<u8> {
    derive_id(8, &["claude_code.tool", tool_use_id])
}

fn prompt_span_id(prompt_id: &str) -> Vec<u8> {
    derive_id(8, &["claude_code.prompt", prompt_id])
}

/// Trace, parent and time for a span made from `rec`. See the module docs: the record's own
/// context when it has one, else a trace per turn under the turn's prompt marker.
fn placement(rec: &LogRecord, is_prompt: bool) -> (Vec<u8>, Vec<u8>, u64) {
    let a = &rec.attributes;
    let (trace, span) = trace_context(rec);
    let prompt = attr_str(a, "prompt.id");
    let trace_id = match trace {
        Some(t) => t.to_vec(),
        None => {
            let session = attr_str(a, "session.id").unwrap_or_default();
            derive_id(
                16,
                &[
                    "claude_code.trace",
                    &session,
                    prompt.as_deref().unwrap_or(""),
                ],
            )
        }
    };
    let parent = match (span, &prompt) {
        (Some(s), _) => s.to_vec(),
        (None, Some(p)) if !is_prompt => prompt_span_id(p),
        _ => Vec::new(),
    };
    (trace_id, parent, record_time(rec))
}

/// A fallback identity for a record that carries none of the ids it should: unique per record,
/// stable across retries.
fn record_key(rec: &LogRecord) -> String {
    let a = &rec.attributes;
    format!(
        "{}|{}|{}",
        attr_str(a, "session.id").unwrap_or_default(),
        attr_str(a, "event.sequence").unwrap_or_default(),
        record_time(rec)
    )
}

/// `start = time − duration_ms`: an event is logged when its operation ends.
fn start_of(time: u64, a: &[KeyValue]) -> u64 {
    let ms = attr_u64(a, "duration_ms").unwrap_or(0);
    time.saturating_sub(ms.saturating_mul(1_000_000))
}

/// Add `kv` unless the record already has that key (a later Claude Code version that sends the
/// `gen_ai.*` spelling itself wins, since it is what the record said).
fn add(attrs: &mut Vec<KeyValue>, kv: KeyValue) {
    if !attrs.iter().any(|x| x.key == kv.key) {
        attrs.push(kv);
    }
}

fn llm_span(ctx: &RecordCtx, rec: &LogRecord, error: Option<String>) -> NormalizedSpan {
    let a = &rec.attributes;
    let (trace_id, parent, time) = placement(rec, false);
    let request_id = attr_str(a, "request_id");
    let key = request_id
        .clone()
        .or_else(|| attr_str(a, "client_request_id"))
        .unwrap_or_else(|| record_key(rec));
    let mut attributes = a.clone();
    add(&mut attributes, kv_str("gen_ai.operation.name", "chat"));
    add(&mut attributes, kv_str("gen_ai.provider.name", "anthropic"));
    if let Some(model) = attr_str(a, "model") {
        add(&mut attributes, kv_str("gen_ai.request.model", &model));
        add(&mut attributes, kv_str("gen_ai.response.model", &model));
    }
    let input = attr_u64(a, "input_tokens");
    let output = attr_u64(a, "output_tokens");
    let cache_read = attr_u64(a, "cache_read_tokens");
    let cache_write = attr_u64(a, "cache_creation_tokens");
    for (k, v) in [
        ("gen_ai.usage.input_tokens", input),
        ("gen_ai.usage.output_tokens", output),
        ("gen_ai.usage.cache_read.input_tokens", cache_read),
        ("gen_ai.usage.cache_creation.input_tokens", cache_write),
    ] {
        if let Some(v) = v {
            add(&mut attributes, kv_int(k, v as i64));
        }
    }
    if input.is_some() || output.is_some() {
        let total = [input, output, cache_read, cache_write]
            .iter()
            .map(|v| v.unwrap_or(0))
            .sum::<u64>();
        add(
            &mut attributes,
            kv_int("gen_ai.usage.total_tokens", total as i64),
        );
    }
    if let Some(cost) = attr_f64(a, "cost_usd").filter(|c| c.is_finite() && *c >= 0.0) {
        add(&mut attributes, kv_f64("gen_ai.usage.cost", cost));
    }
    if let Some(id) = &request_id {
        add(&mut attributes, kv_str("gen_ai.response.id", id));
    }
    if let Some(ms) = attr_f64(a, "ttft_ms").filter(|v| v.is_finite() && *v >= 0.0) {
        add(
            &mut attributes,
            kv_f64("gen_ai.response.time_to_first_chunk", ms / 1000.0),
        );
    }
    add(&mut attributes, kv_str("evald.source", "log"));
    add(
        &mut attributes,
        kv_str(CALL_KEY, &format!("claude_code.llm:{key}")),
    );
    SpanDraft {
        trace_id,
        span_id: llm_span_id(&key),
        parent_span_id: parent,
        name: LLM_SPAN.to_string(),
        start_unix_nano: start_of(time, a),
        end_unix_nano: time,
        status: error.map(Err),
        attributes,
    }
    .finish(ctx)
}

/// The tool an event is about. Claude Code reports every MCP tool as `mcp_tool`; with
/// `OTEL_LOG_TOOL_DETAILS=1` its `tool_parameters` name the server and the tool, and the call is
/// then named the way Claude Code itself names it: `mcp__<server>__<tool>`.
fn tool_name(a: &[KeyValue]) -> Option<String> {
    let name = attr_str(a, "tool_name")?;
    if name != "mcp_tool" {
        return Some(name);
    }
    let details = attr_str(a, "tool_parameters")
        .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok());
    let part = |k: &str| {
        details
            .as_ref()
            .and_then(|d| d.get(k))
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    match (part("mcp_server_name"), part("mcp_tool_name")) {
        (Some(server), Some(tool)) => Some(format!("mcp__{server}__{tool}")),
        _ => Some(name),
    }
}

fn tool_span(ctx: &RecordCtx, rec: &LogRecord, rejected: Option<String>) -> NormalizedSpan {
    let a = &rec.attributes;
    let (trace_id, parent, time) = placement(rec, false);
    let tool_use_id = attr_str(a, "tool_use_id");
    let key = tool_use_id.clone().unwrap_or_else(|| record_key(rec));
    let mut attributes = a.clone();
    add(
        &mut attributes,
        kv_str("gen_ai.operation.name", "execute_tool"),
    );
    if let Some(tool) = tool_name(a) {
        add(&mut attributes, kv_str("gen_ai.tool.name", &tool));
    }
    if let Some(id) = &tool_use_id {
        add(&mut attributes, kv_str("gen_ai.tool.call.id", id));
    }
    add(&mut attributes, kv_str("evald.source", "log"));
    add(
        &mut attributes,
        kv_str(CALL_KEY, &format!("claude_code.tool:{key}")),
    );
    let failed = attr_str(a, "success").as_deref() == Some("false");
    let status = match rejected {
        Some(reason) => Some(Err(reason)),
        None if failed => Some(Err(
            attr_str(a, "error_type").unwrap_or_else(|| "tool failed".into())
        )),
        None => None,
    };
    let start =
        if status.as_ref().is_some_and(|s| s.is_err()) && attr_u64(a, "duration_ms").is_none() {
            time
        } else {
            start_of(time, a)
        };
    SpanDraft {
        trace_id,
        span_id: tool_span_id(&key),
        parent_span_id: parent,
        name: TOOL_SPAN.to_string(),
        start_unix_nano: start,
        end_unix_nano: time,
        status,
        attributes,
    }
    .finish(ctx)
}

fn prompt_span(ctx: &RecordCtx, rec: &LogRecord) -> NormalizedSpan {
    let a = &rec.attributes;
    let (trace_id, parent, time) = placement(rec, true);
    let key = attr_str(a, "prompt.id").unwrap_or_else(|| record_key(rec));
    let mut attributes = a.clone();
    add(&mut attributes, kv_str("evald.source", "log"));
    SpanDraft {
        trace_id,
        span_id: prompt_span_id(&key),
        parent_span_id: parent,
        name: PROMPT_SPAN.to_string(),
        start_unix_nano: time,
        end_unix_nano: time,
        status: None,
        attributes,
    }
    .finish(ctx)
}

/// Claude Code's spans, as received over traces, made to read like the event copies: tokens
/// from its bare `input_tokens` / `output_tokens` / `cache_*_tokens` (the GenAI spellings are
/// absent on 2.1.306), time to first token, and the call key and span id an LLM request shares
/// with its `api_request` event. Called by [`crate::normalize::normalize_request`] for spans in
/// [`TRACING_SCOPE`], before pricing.
pub(crate) fn fix_trace_span(n: &mut NormalizedSpan) {
    match n.name.as_str() {
        LLM_SPAN => {
            let r = &n.raw_attributes;
            let t = &mut n.tokens;
            if t.prompt.is_none() {
                t.prompt = raw_u64(r, "input_tokens");
            }
            if t.completion.is_none() {
                t.completion = raw_u64(r, "output_tokens");
            }
            if t.cache_read.is_none() {
                t.cache_read = raw_u64(r, "cache_read_tokens");
            }
            if t.cache_write.is_none() {
                t.cache_write = raw_u64(r, "cache_creation_tokens");
            }
            if t.total.is_none() && (t.prompt.is_some() || t.completion.is_some()) {
                t.total = Some(
                    [t.prompt, t.completion, t.cache_read, t.cache_write]
                        .iter()
                        .map(|v| v.unwrap_or(0))
                        .sum(),
                );
            }
            if !n
                .raw_attributes
                .contains_key("gen_ai.response.time_to_first_chunk")
            {
                if let Some(ms) = n.raw_attributes.get("ttft_ms").and_then(json_f64) {
                    if ms.is_finite() && ms >= 0.0 {
                        n.raw_attributes.insert(
                            "gen_ai.response.time_to_first_chunk".into(),
                            serde_json::Number::from_f64(ms / 1000.0)
                                .map(Json::Number)
                                .unwrap_or(Json::Null),
                        );
                    }
                }
            }
            let request_id = n
                .raw_attributes
                .get("request_id")
                .and_then(Json::as_str)
                .map(str::to_string);
            if let Some(req) = request_id.filter(|r| !r.is_empty()) {
                let original = std::mem::replace(&mut n.span_id, hex::encode(llm_span_id(&req)));
                n.raw_attributes
                    .insert("evald.otel.span_id".into(), Json::String(original));
                n.raw_attributes.insert(
                    CALL_KEY.into(),
                    Json::String(format!("claude_code.llm:{req}")),
                );
            }
        }
        // A phase of a tool call (its `gen_ai.tool.call.id` would otherwise make it read as a
        // second TOOL span), not a call of its own.
        "claude_code.tool.execution" => n.oi_kind = None,
        TOOL_SPAN => {
            if let Some(id) = n
                .raw_attributes
                .get("tool_use_id")
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            {
                n.raw_attributes.insert(
                    CALL_ANCHOR.into(),
                    Json::String(format!("claude_code.tool:{id}")),
                );
            }
        }
        _ => {}
    }
}

fn json_f64(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn raw_u64(r: &std::collections::BTreeMap<String, Json>, key: &str) -> Option<u64> {
    match r.get(key)? {
        Json::Number(n) => n.as_u64(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::logs::{normalize_logs, LogsPolicy, Outcome};
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;

    pub(crate) const EVENTS_ONLY: &str =
        include_str!("../tests/fixtures/claude-code/2.1.306/events-only.logs.json");
    pub(crate) const TRACED_LOGS: &str =
        include_str!("../tests/fixtures/claude-code/2.1.306/traced.logs.json");
    pub(crate) const TRACED_SPANS: &str =
        include_str!("../tests/fixtures/claude-code/2.1.306/traced.traces.json");

    pub(crate) fn logs(json: &str) -> ExportLogsServiceRequest {
        crate::ingest::decode_otlp_json_as(json.as_bytes()).expect("fixture decodes")
    }

    pub(crate) fn traces(json: &str) -> ExportTraceServiceRequest {
        crate::ingest::decode_otlp_json_as(json.as_bytes()).expect("fixture decodes")
    }

    fn by_name<'a>(spans: &'a [NormalizedSpan], name: &str) -> Vec<&'a NormalizedSpan> {
        spans.iter().filter(|s| s.name == name).collect()
    }

    /// An MCP tool's `tool_result`, as Claude Code 2.1.306 sends it under the Claude Agent SDK
    /// (examples/recipes/claude-agent-sdk), with and without `OTEL_LOG_TOOL_DETAILS=1`.
    #[test]
    fn an_mcp_tool_is_named_by_its_server_and_tool_when_the_details_are_on() {
        let record = |details: bool| {
            let mut attributes = vec![
                kv_str("event.name", "tool_result"),
                kv_str("session.id", "s1"),
                kv_str("prompt.id", "p1"),
                kv_str("tool_name", "mcp_tool"),
                kv_str("tool_use_id", "toolu_1"),
                kv_str("success", "true"),
                kv_str("duration_ms", "31"),
            ];
            if details {
                attributes.push(kv_str(
                    "tool_parameters",
                    r#"{"mcp_server_name":"weather","mcp_tool_name":"get_weather"}"#,
                ));
                attributes.push(kv_str("tool_input", r#"{"city":"Paris"}"#));
            }
            LogRecord {
                time_unix_nano: 1_800_000_000_000_000_000,
                body: Some(opentelemetry_proto::tonic::common::v1::AnyValue {
                    value: Some(
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            "claude_code.tool_result".into(),
                        ),
                    ),
                }),
                attributes,
                ..Default::default()
            }
        };
        let named = |details: bool| {
            let req =
                crate::logs::tests::request(EVENTS_SCOPE, SERVICE_NAME, vec![record(details)]);
            let span = normalize_logs(&req, LogsPolicy::Genai).spans.remove(0);
            (
                crate::agent::tool_name(&span).map(str::to_string),
                crate::agent::tool_arguments(&span),
            )
        };
        assert_eq!(
            named(true),
            (
                Some("mcp__weather__get_weather".to_string()),
                Some(r#"{"city":"Paris"}"#.to_string())
            )
        );
        assert_eq!(named(false), (Some("mcp_tool".to_string()), None));
    }

    #[test]
    fn events_become_llm_tool_and_prompt_spans_with_tokens_and_claude_codes_own_cost() {
        let b = normalize_logs(&logs(EVENTS_ONLY), LogsPolicy::Genai);
        let llm = by_name(&b.spans, LLM_SPAN);
        assert_eq!(llm.len(), 2, "two api_request events");
        let first = llm[0];
        assert_eq!(first.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(first.provider.as_deref(), Some("anthropic"));
        assert_eq!(first.oi_kind.as_deref(), Some("LLM"));
        assert_eq!(
            first.tokens.prompt,
            Some(2),
            "input excludes cache, as reported"
        );
        assert_eq!(first.tokens.completion, Some(268));
        assert_eq!(first.tokens.cache_read, Some(25829));
        assert_eq!(first.tokens.cache_write, Some(9950));
        assert_eq!(first.tokens.total, Some(2 + 268 + 25829 + 9950));
        assert_eq!(
            first.cost_usd,
            Some(0.00238249),
            "Claude Code's estimate, not re-derived"
        );
        assert_eq!(
            first.end_unix_nano - first.start_unix_nano,
            2112 * 1_000_000,
            "duration_ms back to a start time"
        );
        assert_eq!(
            crate::latency::ttft_seconds(&first.raw_attributes),
            Some(1.254)
        );
        assert!(first
            .session_id
            .as_deref()
            .is_some_and(|s| s.starts_with("session-fixture")));

        let tools = by_name(&b.spans, TOOL_SPAN);
        assert_eq!(tools.len(), 2, "Bash and Write tool results");
        assert!(tools.iter().all(|t| t.oi_kind.as_deref() == Some("TOOL")));
        let bash = tools
            .iter()
            .find(|t| t.raw_attributes["gen_ai.tool.name"] == "Bash")
            .unwrap();
        assert_eq!(
            bash.end_unix_nano - bash.start_unix_nano,
            103 * 1_000_000,
            "duration_ms arrives as a string on tool events"
        );
        assert_eq!(bash.status_code, 0, "success leaves the status unset");

        assert_eq!(by_name(&b.spans, PROMPT_SPAN).len(), 1);
        // Two accepted tool decisions and one assistant response are folded; managed-settings
        // and plugin events are not GenAI facts.
        assert_eq!(b.outcomes[2], 3, "folded");
        assert_eq!(b.outcomes[3], 4, "unmapped");
        assert_eq!(b.outcomes.iter().sum::<u64>(), b.records as u64);
    }

    #[test]
    fn a_turns_spans_share_its_trace_and_hang_under_the_record_context() {
        let b = normalize_logs(&logs(EVENTS_ONLY), LogsPolicy::Genai);
        let traces: std::collections::BTreeSet<_> =
            b.spans.iter().map(|s| s.trace_id.as_str()).collect();
        assert_eq!(traces.len(), 1, "one turn, one trace: {traces:?}");
    }

    #[test]
    fn without_a_trace_context_a_turn_gets_its_own_trace_under_its_prompt_marker() {
        let mut req = logs(EVENTS_ONLY);
        for rl in &mut req.resource_logs {
            for sl in &mut rl.scope_logs {
                for r in &mut sl.log_records {
                    r.trace_id.clear();
                    r.span_id.clear();
                }
            }
        }
        let b = normalize_logs(&req, LogsPolicy::Genai);
        let prompt = by_name(&b.spans, PROMPT_SPAN)[0];
        assert!(prompt.parent_span_id.is_none(), "the marker is the root");
        for s in by_name(&b.spans, LLM_SPAN)
            .into_iter()
            .chain(by_name(&b.spans, TOOL_SPAN))
        {
            assert_eq!(s.trace_id, prompt.trace_id);
            assert_eq!(s.parent_span_id.as_deref(), Some(prompt.span_id.as_str()));
        }
    }

    #[test]
    fn a_rejected_tool_and_a_failed_request_carry_an_error_status() {
        let mut req = logs(EVENTS_ONLY);
        let decision = req
            .resource_logs
            .iter_mut()
            .flat_map(|rl| rl.scope_logs.iter_mut())
            .flat_map(|sl| sl.log_records.iter_mut())
            .find(|r| attr_str(&r.attributes, "event.name").as_deref() == Some("tool_decision"))
            .unwrap();
        for kv in &mut decision.attributes {
            if kv.key == "decision" {
                *kv = kv_str("decision", "reject");
            }
            if kv.key == "source" {
                *kv = kv_str("source", "user_reject");
            }
        }
        req.resource_logs[0].scope_logs[0]
            .log_records
            .push(LogRecord {
                time_unix_nano: 1_800_000_000_000_000_000,
                attributes: vec![
                    kv_str("event.name", "api_error"),
                    kv_str("session.id", "s"),
                    kv_str("model", "claude-haiku-5-5"),
                    kv_str("error", "overloaded_error"),
                    kv_int("duration_ms", 40),
                    kv_str("client_request_id", "c-1"),
                ],
                ..Default::default()
            });
        let b = normalize_logs(&req, LogsPolicy::Genai);
        let rejected: Vec<_> = by_name(&b.spans, TOOL_SPAN)
            .into_iter()
            .filter(|s| s.status_code == 2)
            .collect();
        assert_eq!(rejected.len(), 1);
        assert_eq!(
            rejected[0].status_message.as_deref(),
            Some("rejected (user_reject)")
        );
        let failed = by_name(&b.spans, LLM_SPAN)
            .into_iter()
            .find(|s| s.status_code == 2)
            .expect("the api_error span");
        assert_eq!(failed.status_message.as_deref(), Some("overloaded_error"));
        assert_eq!(failed.tokens.total, None);
    }

    #[test]
    fn traced_spans_read_their_bare_token_counts_and_take_the_events_span_id() {
        let spans = crate::normalize::normalize_request(&traces(TRACED_SPANS));
        let llm = by_name(&spans, LLM_SPAN);
        assert_eq!(llm.len(), 3);
        for s in &llm {
            assert!(
                s.tokens.prompt.is_some() && s.tokens.completion.is_some(),
                "{s:?}"
            );
            let req = s.raw_attributes["request_id"].as_str().unwrap();
            assert_eq!(s.span_id, hex::encode(llm_span_id(req)));
            assert!(s.raw_attributes.contains_key("evald.otel.span_id"));
            assert_eq!(s.raw_attributes[CALL_KEY], format!("claude_code.llm:{req}"));
        }
        assert!(by_name(&spans, TOOL_SPAN)[0]
            .raw_attributes
            .contains_key(CALL_ANCHOR));
        // Every call the events describe has the same span id on the trace side.
        let events = normalize_logs(&logs(TRACED_LOGS), LogsPolicy::Genai);
        for e in by_name(&events.spans, LLM_SPAN) {
            assert!(
                llm.iter()
                    .any(|s| s.span_id == e.span_id && s.trace_id == e.trace_id),
                "event {} has no span twin",
                e.span_id
            );
        }
    }

    #[test]
    fn records_from_other_services_are_not_claude_codes() {
        let mut req = logs(EVENTS_ONLY);
        for rl in &mut req.resource_logs {
            rl.resource = None;
            for sl in &mut rl.scope_logs {
                sl.scope = None;
            }
        }
        let b = normalize_logs(&req, LogsPolicy::Genai);
        assert!(b.spans.is_empty());
        assert_eq!(b.outcomes[Outcome::Unmapped as usize], b.records as u64);
    }
}
