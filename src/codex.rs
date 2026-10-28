//! Codex CLI's telemetry, mapped (0.4.0, `ROADMAP-0.4-0.6.md` 4.2).
//!
//! Codex exports **events** over the OTLP logs signal (`[otel] exporter`) and, separately,
//! **spans** (`[otel] trace_exporter`). The mapping was written against what version 0.163.0
//! sends (`tests/fixtures/codex/0.163.0/`, captured by `scripts/capture-coding-agent.sh` from a
//! real `codex exec` session) and against Codex's own source at that tag
//! (`codex-rs/otel/src/events/session_telemetry.rs`), not against third-party guides.
//!
//! # Events → spans
//!
//! | Event | Becomes |
//! |---|---|
//! | `sse_event`, `event.kind = response.completed` with token counts | an LLM span `codex.llm_request`: model, tokens, time to first token |
//! | `api_request` / `websocket_request` that failed, `sse_event` with `error.message` | the same span shape with an error status |
//! | `tool_result` | a TOOL span `codex.tool`: tool name (`mcp__<server>__<tool>` for MCP), call id, arguments, duration, an error status when it failed or was refused |
//! | `user_prompt` | a zero-duration `codex.user_prompt` marker, one per turn |
//! | `conversation_starts`, `tool_decision`, a successful request, every other `sse_event` | folded: remembered, or covered by the records above |
//!
//! Every other event (`startup_phase`, `turn_ttft`, `turn_cost`, `sandbox_outcome`, …) is left
//! to the logs policy. A record's own attributes are kept on its span, plus their `gen_ai.*`
//! spellings. Codex fills the OTLP `event_name` field with a `tracing` callsite; the name is the
//! `event.name` attribute ([`crate::logs::event_name`]).
//!
//! **Tokens** are OpenAI's: `input_token_count` *includes* the cached tokens, and
//! `output_token_count` the reasoning tokens. `tool_token_count` is, despite its name, the total.
//!
//! **A command that fails is a successful tool call to Codex**: `exec_command` reports
//! `success = true` with `Process exited with code 3` in its output. When the output preview
//! carries that line, the span gets an error status `exit code 3`; with `[otel] tool_result =
//! { max_bytes = 0 }` (what `evald connect codex` sets, to keep command output out of evald:
//! privacy first, `ROADMAP-0.4-0.6.md` D11) there is no preview, and Codex's verdict stands.
//! evald does not guess a failure from the command or the time it took.
//!
//! # Where a span sits
//!
//! A turn is one trace, under its `codex.user_prompt` marker. Codex's records carry a trace
//! context, but not one to place by: with its spans exported, each turn has a trace of its own;
//! without (what `evald connect codex` sets up), every turn of a conversation shares one. So
//! each `user_prompt` starts a trace (derived from the conversation and the prompt's time),
//! remembered for the conversation ([`crate::logs::Recall`]), and every span after it sits in
//! that trace under the marker. A record's own context is kept as `evald.otel.trace_id` /
//! `evald.otel.span_id`, the way to Codex's spans when they are exported.
//!
//! The usage at the end of a model call (`response.completed`) is logged outside any span, and
//! is the record that matters most. The request it ends (the last `api_request` /
//! `websocket_request` of its conversation, remembered across export requests: a stream
//! outlasts the exporter's flush) gives it its start and its `evald.otel.*` context. With
//! nothing remembered (a restart, another node took the request), a span sits in a trace of its
//! conversation's own, and a model call has no duration. Tokens and session are the record's own
//! either way.
//!
//! # Spans
//!
//! Codex's spans are its internals (hundreds a turn: file reads, auth, tool routing). They are
//! kept as they arrive, but never read as model or tool calls, which its events already count:
//! the one that carries usage (`handle_responses`, on the stream's completion) has it set aside
//! (`evald.usage = "copy"`), and none is part of a session's rollup (its `session_id` is not
//! set; the attribute stays) ([`fix_trace_span`]). The events are what `evald connect codex`
//! turns on; the spans are optional, and show where a turn's time went.

use std::collections::HashMap;
use std::sync::OnceLock;

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::LogRecord;

use crate::logs::{
    derive_id, kv_int, kv_str, record_time, trace_context, Mapping, Recall, RecordCtx, SpanDraft,
};
use crate::model::NormalizedSpan;
use crate::normalize::{attr_f64, attr_str, attr_u64};

const LLM_SPAN: &str = "codex.llm_request";
const TOOL_SPAN: &str = "codex.tool";
const PROMPT_SPAN: &str = "codex.user_prompt";

/// What is remembered about one conversation between records.
#[derive(Clone, Default)]
pub(crate) struct Conversation {
    /// The turn in progress: its trace and its prompt marker.
    turn: Option<(Vec<u8>, Vec<u8>)>,
    /// The model request most recently sent: where the usage that ends it belongs.
    request: Option<Request>,
    /// The model provider (`conversation_starts`).
    provider: Option<String>,
    /// Tool calls refused by a decision, and why: the call's result follows the decision.
    refused: Vec<(String, String)>,
}

#[derive(Clone)]
struct Request {
    trace_id: Vec<u8>,
    span_id: Vec<u8>,
    start: u64,
}

/// Refusals remembered per conversation, oldest forgotten first.
const REFUSED_KEPT: usize = 16;

fn recall() -> &'static Recall<Conversation> {
    static RECALL: OnceLock<Recall<Conversation>> = OnceLock::new();
    RECALL.get_or_init(Recall::new)
}

/// Is this record Codex's? Its events are logged under the `codex_otel.*` scopes, by a service
/// named for the client (`codex_exec`, `codex_cli_rs`, …).
fn is_mine(ctx: &RecordCtx) -> bool {
    ctx.scope_name.is_some_and(|s| s.starts_with("codex_otel"))
        || ctx
            .service_name
            .as_deref()
            .is_some_and(|s| s.starts_with("codex"))
}

/// Is this span Codex's? Called once per scope by [`crate::normalize::normalize_request`].
pub(crate) fn is_trace_scope(service_name: Option<&str>) -> bool {
    service_name.is_some_and(|s| s.starts_with("codex"))
}

/// Map one Codex event record ([`crate::logs`] calls this for every record).
pub(crate) fn map_log(
    ctx: &RecordCtx,
    rec: &LogRecord,
    name: Option<&str>,
    local: &mut HashMap<String, Conversation>,
) -> Mapping {
    if !is_mine(ctx) {
        return Mapping::NotMine;
    }
    let Some(event) = name.and_then(|n| n.strip_prefix("codex.")) else {
        return Mapping::NotMine;
    };
    let a = &rec.attributes;
    let conv = attr_str(a, "conversation.id").unwrap_or_default();
    match event {
        "conversation_starts" => {
            if let Some(p) = attr_str(a, "provider_name").filter(|p| !p.is_empty()) {
                recall().update(local, &conv, |c| c.provider = Some(p.to_ascii_lowercase()));
            }
            Mapping::Folded
        }
        "user_prompt" => {
            let time = record_time(rec).to_string();
            let turn = (
                derive_id(16, &["codex.turn", &conv, &time]),
                derive_id(8, &["codex.prompt", &conv, &time]),
            );
            let span = prompt_span(ctx, rec, &conv, &turn);
            recall().update(local, &conv, |c| c.turn = Some(turn));
            Mapping::Span(Box::new(span))
        }
        // A request to anything but the Responses API (a model list, Codex's turn-cost lookup)
        // is not a model call.
        "api_request" if attr_str(a, "endpoint").is_some_and(|e| !e.contains("responses")) => {
            Mapping::Folded
        }
        "api_request" | "websocket_request" => match request_error(event, a) {
            Some(msg) => Mapping::Span(Box::new(llm_span(ctx, rec, &conv, Some(msg), local))),
            None => {
                let (trace, span) = trace_context(rec);
                let time = record_time(rec);
                let request = Request {
                    trace_id: trace.map(<[u8]>::to_vec).unwrap_or_default(),
                    span_id: span.map(<[u8]>::to_vec).unwrap_or_default(),
                    start: start_of(time, a),
                };
                recall().update(local, &conv, |c| c.request = Some(request));
                Mapping::Folded
            }
        },
        "sse_event" => {
            let kind = attr_str(a, "event.kind");
            let ends_call = matches!(
                kind.as_deref(),
                None | Some("response.completed" | "response.failed")
            );
            if let Some(err) = attr_str(a, "error.message").filter(|_| ends_call) {
                Mapping::Span(Box::new(llm_span(ctx, rec, &conv, Some(err), local)))
            } else if kind.as_deref() == Some("response.completed")
                && attr_u64(a, "input_token_count").is_some()
            {
                Mapping::Span(Box::new(llm_span(ctx, rec, &conv, None, local)))
            } else {
                Mapping::Folded
            }
        }
        "tool_decision" => {
            let decision = attr_str(a, "decision").unwrap_or_default();
            if !decision.starts_with("approved") {
                if let Some(call) = attr_str(a, "call_id") {
                    let source = attr_str(a, "source").unwrap_or_else(|| "unknown".into());
                    let why = format!("{decision} ({source})");
                    recall().update(local, &conv, |c| {
                        c.refused.push((call, why));
                        if c.refused.len() > REFUSED_KEPT {
                            c.refused.remove(0);
                        }
                    });
                }
            }
            Mapping::Folded
        }
        "tool_result" => Mapping::Span(Box::new(tool_span(ctx, rec, &conv, local))),
        _ => Mapping::NotMine,
    }
}

/// Why a request record failed, or `None` when it succeeded.
fn request_error(event: &str, a: &[KeyValue]) -> Option<String> {
    if let Some(msg) = attr_str(a, "error.message").filter(|m| !m.is_empty()) {
        return Some(msg);
    }
    if event == "websocket_request" {
        return (attr_str(a, "success").as_deref() == Some("false"))
            .then(|| "websocket request failed".into());
    }
    match attr_u64(a, "http.response.status_code") {
        Some(code) if (200..300).contains(&code) => None,
        Some(code) => Some(format!("HTTP {code}")),
        // "none": no response at all.
        None => Some("no response".into()),
    }
}

/// `start = time − duration_ms`: an event is logged when its operation ends.
fn start_of(time: u64, a: &[KeyValue]) -> u64 {
    let ms = attr_u64(a, "duration_ms").unwrap_or(0);
    time.saturating_sub(ms.saturating_mul(1_000_000))
}

/// Add `kv` unless the record already has that key.
fn add(attrs: &mut Vec<KeyValue>, kv: KeyValue) {
    if !attrs.iter().any(|x| x.key == kv.key) {
        attrs.push(kv);
    }
}

/// The turn a span belongs to: its trace and parent (the prompt marker). With no turn
/// remembered, a trace of the conversation's own.
fn placement(local: &HashMap<String, Conversation>, conv: &str) -> (Vec<u8>, Vec<u8>) {
    recall()
        .get(local, conv)
        .and_then(|c| c.turn)
        .unwrap_or_else(|| (derive_id(16, &["codex.trace", conv]), Vec::new()))
}

/// A record's own trace context, when it has one.
fn own_context(rec: &LogRecord) -> Option<(Vec<u8>, Vec<u8>)> {
    match trace_context(rec) {
        (Some(t), Some(s)) => Some((t.to_vec(), s.to_vec())),
        _ => None,
    }
}

/// The attributes every span made here carries: the record's, the trace context of what it
/// describes (`own`: kept, since the span is placed by turn), and the conversation.
fn common(a: &[KeyValue], conv: &str, own: Option<&(Vec<u8>, Vec<u8>)>) -> Vec<KeyValue> {
    let mut attributes = a.to_vec();
    if let Some((t, s)) = own {
        add(
            &mut attributes,
            kv_str("evald.otel.trace_id", &hex::encode(t)),
        );
        add(
            &mut attributes,
            kv_str("evald.otel.span_id", &hex::encode(s)),
        );
    }
    if !conv.is_empty() {
        add(&mut attributes, kv_str("gen_ai.conversation.id", conv));
    }
    add(&mut attributes, kv_str("evald.source", "log"));
    attributes
}

fn llm_span(
    ctx: &RecordCtx,
    rec: &LogRecord,
    conv: &str,
    error: Option<String>,
    local: &HashMap<String, Conversation>,
) -> NormalizedSpan {
    let a = &rec.attributes;
    let time = record_time(rec);
    let remembered = recall().get(local, conv).unwrap_or_default();
    let (trace_id, parent) = placement(local, conv);
    let (start, own) = match (own_context(rec), &remembered.request) {
        // A failed request is its own record, with its own duration.
        (Some(own), _) => (start_of(time, a), Some(own)),
        // The end of a stream: it started when the request it ends did.
        (None, Some(r)) => (
            r.start.min(time),
            (!r.trace_id.is_empty()).then(|| (r.trace_id.clone(), r.span_id.clone())),
        ),
        (None, None) => (time, None),
    };
    let mut attributes = common(a, conv, own.as_ref());
    add(&mut attributes, kv_str("gen_ai.operation.name", "chat"));
    if let Some(p) = &remembered.provider {
        add(&mut attributes, kv_str("gen_ai.provider.name", p));
    }
    if let Some(model) = attr_str(a, "model") {
        add(&mut attributes, kv_str("gen_ai.request.model", &model));
        add(&mut attributes, kv_str("gen_ai.response.model", &model));
    }
    for (k, from) in [
        ("gen_ai.usage.input_tokens", "input_token_count"),
        ("gen_ai.usage.output_tokens", "output_token_count"),
        ("gen_ai.usage.cache_read.input_tokens", "cached_token_count"),
        (
            "gen_ai.usage.cache_creation.input_tokens",
            "cache_write_token_count",
        ),
        (
            "gen_ai.usage.reasoning.output_tokens",
            "reasoning_token_count",
        ),
        ("gen_ai.usage.total_tokens", "tool_token_count"),
    ] {
        if let Some(v) = attr_u64(a, from) {
            add(&mut attributes, kv_int(k, v as i64));
        }
    }
    if let Some(ms) = attr_f64(a, "ttft_ms").filter(|v| v.is_finite() && *v >= 0.0) {
        add(
            &mut attributes,
            crate::logs::kv_f64("gen_ai.response.time_to_first_chunk", ms / 1000.0),
        );
    }
    let attempt = attr_str(a, "attempt").unwrap_or_default();
    SpanDraft {
        trace_id,
        span_id: derive_id(8, &["codex.llm", conv, &time.to_string(), &attempt]),
        parent_span_id: parent,
        name: LLM_SPAN.to_string(),
        start_unix_nano: start,
        end_unix_nano: time,
        status: error.map(Err),
        attributes,
    }
    .finish(ctx)
}

/// The tool a result is about, named as Codex names it to the model: an MCP tool is
/// `mcp__<server>__<tool>` (its namespace is `mcp__<server>`).
fn tool_name(a: &[KeyValue]) -> Option<String> {
    let name = attr_str(a, "tool_name")?;
    match attr_str(a, "tool_namespace") {
        Some(ns) if ns.starts_with("mcp__") => Some(format!("{ns}__{name}")),
        _ => Some(name),
    }
}

/// `Process exited with code N` in a command's output preview: the code, when it is not 0.
fn exit_code(a: &[KeyValue]) -> Option<i64> {
    let output = attr_str(a, "output")?;
    let code = output
        .lines()
        .find_map(|l| l.strip_prefix("Process exited with code "))?
        .trim()
        .parse::<i64>()
        .ok()?;
    (code != 0).then_some(code)
}

fn tool_span(
    ctx: &RecordCtx,
    rec: &LogRecord,
    conv: &str,
    local: &mut HashMap<String, Conversation>,
) -> NormalizedSpan {
    let a = &rec.attributes;
    let time = record_time(rec);
    let (trace_id, parent) = placement(local, conv);
    let call_id = attr_str(a, "call_id");
    let key = call_id.clone().unwrap_or_else(|| format!("{conv}|{time}"));
    let mut attributes = common(a, conv, own_context(rec).as_ref());
    add(
        &mut attributes,
        kv_str("gen_ai.operation.name", "execute_tool"),
    );
    if let Some(tool) = tool_name(a) {
        add(&mut attributes, kv_str("gen_ai.tool.name", &tool));
    }
    if let Some(id) = &call_id {
        add(&mut attributes, kv_str("gen_ai.tool.call.id", id));
    }
    if let Some(args) = attr_str(a, "arguments").filter(|s| !s.is_empty()) {
        add(&mut attributes, kv_str("gen_ai.tool.call.arguments", &args));
    }
    if let Some(server) = attr_str(a, "mcp_server").filter(|s| !s.is_empty()) {
        add(&mut attributes, kv_str("mcp.server.name", &server));
    }
    // A refusal is decided before the call, and remembered until its result arrives.
    let mut refused = None;
    if let Some(id) = &call_id {
        if recall()
            .get(local, conv)
            .is_some_and(|c| c.refused.iter().any(|(c, _)| c == id))
        {
            recall().update(local, conv, |c| {
                if let Some(i) = c.refused.iter().position(|(c, _)| c == id) {
                    refused = Some(c.refused.remove(i).1);
                }
            });
        }
    }
    let status = if let Some(why) = refused {
        Some(Err(why))
    } else if attr_str(a, "success").as_deref() == Some("false") {
        Some(Err("tool failed".to_string()))
    } else {
        exit_code(a).map(|c| Err(format!("exit code {c}")))
    };
    SpanDraft {
        trace_id,
        span_id: derive_id(8, &["codex.tool", &key]),
        parent_span_id: parent,
        name: TOOL_SPAN.to_string(),
        start_unix_nano: start_of(time, a),
        end_unix_nano: time,
        status,
        attributes,
    }
    .finish(ctx)
}

fn prompt_span(
    ctx: &RecordCtx,
    rec: &LogRecord,
    conv: &str,
    (trace_id, span_id): &(Vec<u8>, Vec<u8>),
) -> NormalizedSpan {
    let time = record_time(rec);
    SpanDraft {
        trace_id: trace_id.clone(),
        span_id: span_id.clone(),
        parent_span_id: Vec::new(),
        name: PROMPT_SPAN.to_string(),
        start_unix_nano: time,
        end_unix_nano: time,
        status: None,
        attributes: common(&rec.attributes, conv, own_context(rec).as_ref()),
    }
    .finish(ctx)
}

/// Codex's spans, as received over traces: the copy of a model call's usage that its
/// `handle_responses` span records on the stream's completion is set aside, since the
/// `response.completed` event reports the same call and counts it, and no span is part of a
/// session's rollup, whose turns and calls the events give. Called by
/// [`crate::normalize::normalize_request`] for spans of a Codex service, before pricing.
pub(crate) fn fix_trace_span(n: &mut NormalizedSpan) {
    if n.name == "handle_responses" && (!n.tokens.is_empty() || n.cost_usd.is_some()) {
        crate::agent::set_aside(n, crate::agent::USAGE_COPY);
    }
    n.session_id = None;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::claude_code::tests::{logs, traces};
    use crate::logs::{normalize_logs, LogsPolicy, Outcome};

    pub(crate) const SESSION_LOGS: &str =
        include_str!("../tests/fixtures/codex/0.163.0/session.logs.json");
    const SESSION_SPANS: &str = include_str!("../tests/fixtures/codex/0.163.0/session.traces.json");

    fn by_name<'a>(spans: &'a [NormalizedSpan], name: &str) -> Vec<&'a NormalizedSpan> {
        spans.iter().filter(|s| s.name == name).collect()
    }

    #[test]
    fn every_model_call_is_one_llm_span_with_openai_token_semantics() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let llm = by_name(&b.spans, LLM_SPAN);
        assert_eq!(llm.len(), 7, "five calls in turn 1, two in turn 2");
        for s in &llm {
            assert_eq!(s.oi_kind.as_deref(), Some("LLM"));
            assert_eq!(s.model.as_deref(), Some("gpt-5.5"));
            assert_eq!(s.provider.as_deref(), Some("stub"), "conversation_starts");
            assert_eq!(
                s.tokens.prompt,
                Some(120),
                "input includes the cached tokens"
            );
            assert_eq!(s.tokens.completion, Some(24));
            assert_eq!(s.tokens.total, Some(144), "tool_token_count is the total");
            assert!(s.cost_usd.is_some_and(|c| c > 0.0), "priced from the table");
            assert_eq!(s.status_code, 0);
            assert_eq!(s.session_id.as_deref(), Some("session-fixture-1"));
        }
        assert_eq!(
            llm[0].tokens.cache_read,
            Some(0),
            "a turn's first call is cold"
        );
        assert_eq!(llm[1].tokens.cache_read, Some(64));
        // Cached input is cheaper: a warm call costs less than the cold one.
        assert!(llm[1].cost_usd < llm[0].cost_usd);
    }

    #[test]
    fn a_turn_is_one_trace_under_its_prompt_and_a_call_starts_with_its_request() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let prompts = by_name(&b.spans, PROMPT_SPAN);
        assert_eq!(prompts.len(), 2, "two turns");
        assert_ne!(prompts[0].trace_id, prompts[1].trace_id, "a trace per turn");
        for s in b.spans.iter().filter(|s| s.name != PROMPT_SPAN) {
            let prompt = prompts
                .iter()
                .find(|p| p.trace_id == s.trace_id)
                .unwrap_or_else(|| panic!("{} outside a turn", s.name));
            assert_eq!(
                s.parent_span_id.as_deref(),
                Some(prompt.span_id.as_str()),
                "under the turn's prompt"
            );
            assert!(
                s.raw_attributes.contains_key("evald.otel.span_id"),
                "{} keeps the context of what it describes",
                s.name
            );
        }
        for s in by_name(&b.spans, LLM_SPAN) {
            let ms = (s.end_unix_nano - s.start_unix_nano) / 1_000_000;
            assert!(
                ms >= 9,
                "from the request's start to the stream's end: {ms} ms"
            );
        }
    }

    /// Without its spans exported, Codex gives every turn of a conversation one trace context
    /// (seen live with `evald connect codex`'s config); the turns are still turns.
    #[test]
    fn turns_that_share_one_trace_context_are_still_a_trace_each() {
        let mut req = logs(SESSION_LOGS);
        for rl in &mut req.resource_logs {
            for sl in &mut rl.scope_logs {
                for r in &mut sl.log_records {
                    if !r.trace_id.is_empty() {
                        r.trace_id = vec![0xab; 16];
                    }
                }
            }
        }
        let b = normalize_logs(&req, LogsPolicy::Genai);
        let r = crate::sessions::rollups(&b.spans, false).remove(0);
        assert_eq!(r.trace_count, 2);
        assert_eq!(r.steps_per_trace, 3.5, "seven model calls over two turns");
    }

    #[test]
    fn a_usage_record_whose_request_was_in_an_earlier_export_is_still_placed() {
        // Split the session after every record: each export request is mapped on its own, as
        // happens when a stream outlasts the exporter's flush interval. The conversation gets an
        // id of its own: what crosses requests is this node's memory, shared with every test.
        let full = logs(&SESSION_LOGS.replace("session-fixture-1", "codex-split-export"));
        let whole = normalize_logs(&full, LogsPolicy::Genai);
        let mut split = Vec::new();
        for rl in &full.resource_logs {
            for sl in &rl.scope_logs {
                for r in &sl.log_records {
                    let mut req = full.clone();
                    req.resource_logs = vec![rl.clone()];
                    req.resource_logs[0].scope_logs = vec![sl.clone()];
                    req.resource_logs[0].scope_logs[0].log_records = vec![r.clone()];
                    split.extend(normalize_logs(&req, LogsPolicy::Genai).spans);
                }
            }
        }
        let place = |spans: &[NormalizedSpan]| -> Vec<(String, String, Option<String>)> {
            by_name(spans, LLM_SPAN)
                .iter()
                .map(|s| {
                    (
                        s.span_id.clone(),
                        s.trace_id.clone(),
                        s.parent_span_id.clone(),
                    )
                })
                .collect()
        };
        assert_eq!(place(&split), place(&whole.spans));
    }

    #[test]
    fn tool_calls_are_named_like_codex_names_them_and_fail_when_they_failed() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let tools = by_name(&b.spans, TOOL_SPAN);
        let summary: Vec<(String, i32, Option<String>)> = tools
            .iter()
            .map(|t| {
                (
                    crate::agent::tool_name(t).unwrap().to_string(),
                    t.status_code,
                    t.status_message.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("exec_command".into(), 0, None),
                ("tool_search".into(), 0, None),
                ("mcp__weather__get_weather".into(), 0, None),
                ("exec_command".into(), 2, Some("exit code 3".into())),
                (
                    "mcp__weather__get_weather".into(),
                    2,
                    Some("denied (User)".into())
                ),
            ]
        );
        let mcp = tools[2];
        assert_eq!(mcp.oi_kind.as_deref(), Some("TOOL"));
        assert_eq!(mcp.raw_attributes["mcp.server.name"], "weather");
        assert_eq!(
            crate::agent::tool_arguments(mcp).as_deref(),
            Some(r#"{"city": "Paris"}"#)
        );
        let ms: u64 = mcp.raw_attributes["duration_ms"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(mcp.end_unix_nano - mcp.start_unix_nano, ms * 1_000_000);
    }

    /// `evald connect codex` keeps tool output out (privacy first, ROADMAP D11): Codex then sends
    /// only a truncation marker (seen live), and a failed command stays Codex's success.
    #[test]
    fn without_the_output_a_failed_command_stays_a_success() {
        let mut req = logs(SESSION_LOGS);
        for rl in &mut req.resource_logs {
            for sl in &mut rl.scope_logs {
                for r in &mut sl.log_records {
                    for kv in &mut r.attributes {
                        if kv.key == "output" {
                            *kv = kv_str("output", "[... telemetry preview truncated ...]");
                        }
                    }
                }
            }
        }
        let b = normalize_logs(&req, LogsPolicy::Genai);
        let failed: Vec<_> = by_name(&b.spans, TOOL_SPAN)
            .into_iter()
            .filter(|t| t.status_code == 2)
            .map(|t| t.status_message.clone().unwrap_or_default())
            .collect();
        assert_eq!(
            failed,
            vec!["denied (User)".to_string()],
            "only the refusal"
        );
    }

    #[test]
    fn the_session_reads_as_two_turns_of_calls_tools_and_a_refusal() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let r = crate::sessions::rollups(&b.spans, false).remove(0);
        assert_eq!(r.session_id, "session-fixture-1");
        assert_eq!(r.trace_count, 2, "two turns");
        assert_eq!(r.llm_calls, 7);
        assert_eq!(r.llm_errors, 0);
        assert_eq!(r.tool_calls, 5);
        assert_eq!(r.tool_errors, 2, "exit 3 and the refused MCP call");
        assert_eq!(r.models, vec!["gpt-5.5".to_string()]);
        assert!(r.cost_usd.is_some_and(|c| c > 0.0));
        // Every record is accounted for, and no non-GenAI event became a span.
        assert_eq!(b.outcomes.iter().sum::<u64>(), b.records as u64);
        assert_eq!(
            b.outcomes[Outcome::Span.index()],
            (7 + 5 + 2) as u64,
            "LLM, tool and prompt spans only"
        );
    }

    #[test]
    fn a_failed_request_is_a_failed_model_call_in_its_own_context() {
        let rec = |status: &str, attempt: &str| LogRecord {
            observed_time_unix_nano: 1_800_000_000_000_000_000,
            trace_id: vec![7; 16],
            span_id: vec![8; 8],
            attributes: vec![
                kv_str("event.name", "codex.api_request"),
                kv_str("conversation.id", "c1"),
                kv_str("model", "gpt-5.5"),
                kv_str("duration_ms", "250"),
                kv_str("http.response.status_code", status),
                kv_str("attempt", attempt),
            ],
            ..Default::default()
        };
        let mut records = vec![rec("429", "0"), rec("200", "1")];
        let mut other = rec("404", "0");
        other
            .attributes
            .push(kv_str("endpoint", "/analytics/codex/turn-costs"));
        records.push(other);
        let req = crate::logs::tests::request("codex_otel.log_only", "codex_exec", records);
        let b = normalize_logs(&req, LogsPolicy::Genai);
        assert_eq!(
            b.spans.len(),
            1,
            "the retry that succeeded waits for its usage; a request elsewhere is no model call"
        );
        let s = &b.spans[0];
        assert_eq!(s.name, LLM_SPAN);
        assert_eq!(s.status_code, 2);
        assert_eq!(s.status_message.as_deref(), Some("HTTP 429"));
        assert_eq!(
            s.raw_attributes["evald.otel.trace_id"],
            hex::encode([7u8; 16])
        );
        assert_eq!(s.end_unix_nano - s.start_unix_nano, 250 * 1_000_000);
    }

    #[test]
    fn traced_spans_add_no_model_or_tool_calls() {
        let spans = crate::normalize::normalize_request(&traces(SESSION_SPANS));
        let copies: Vec<_> = spans
            .iter()
            .filter(|s| s.raw_attributes.contains_key(crate::agent::USAGE_KEY))
            .collect();
        assert_eq!(copies.len(), 7, "one usage copy per model call");
        assert!(
            spans.iter().all(|s| !s.is_llm_span()),
            "no span is a model call"
        );
        assert!(spans
            .iter()
            .all(|s| s.tokens.is_empty() && s.cost_usd.is_none()));
        assert!(
            spans.iter().all(|s| s.session_id.is_none()),
            "outside the rollup"
        );
        // With both signals on, the session counts each call, and each turn, once.
        let mut all = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai).spans;
        all.extend(spans);
        let r = crate::sessions::rollups(&all, false).remove(0);
        assert_eq!((r.llm_calls, r.tool_calls, r.trace_count), (7, 5, 2));
    }
}
