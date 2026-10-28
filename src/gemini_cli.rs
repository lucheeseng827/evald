//! Gemini CLI's telemetry, mapped (0.4.0, `ROADMAP-0.4-0.6.md` 4.2).
//!
//! Gemini CLI exports **events** over the OTLP logs signal whenever its telemetry is on, and
//! **spans** too with `telemetry.traces`. The mapping was written against what version 0.65.0
//! sends (`tests/fixtures/gemini-cli/0.65.0/`, captured by `scripts/capture-coding-agent.sh`
//! from a real `gemini -p` session), not against its documentation.
//!
//! # Events → spans
//!
//! | Event | Becomes |
//! |---|---|
//! | `gemini_cli.api_response` | an LLM span `gemini_cli.llm_request`: model, tokens, duration |
//! | `gemini_cli.api_error` | the same span shape with an error status |
//! | `gemini_cli.tool_call` | a TOOL span `gemini_cli.tool`: tool name (`mcp__<server>__<tool>` for MCP), duration, an error status when it failed or was rejected |
//! | `gemini_cli.user_prompt` | a zero-duration `gemini_cli.user_prompt` marker, one per turn |
//! | `gemini_cli.api_request`, `gen_ai.client.inference.operation.details` | folded: remembered, or covered by the records above |
//!
//! Every other event (`config`, `model_routing`, `startup_stats`, …) is left to the logs policy.
//! A record's own attributes are kept on its span, plus their `gen_ai.*` spellings.
//!
//! **Tokens** are Gemini's: `input_token_count` *includes* the cached tokens, and the thinking
//! tokens (`thoughts_token_count`) come *on top of* `output_token_count`; `total_token_count`
//! adds them all up. They are mapped as such (reasoning tokens, a stated total), which is what
//! lets [`crate::price`] price the thinking tokens once, at the output rate. Tool-use prompt
//! tokens (`tool_token_count`) are input.
//!
//! **The GenAI convention's event** (`gen_ai.client.inference.operation.details`, one when a
//! call starts and one when it ends) describes the same calls with less (no cache or thinking
//! counts) and carries Gemini CLI's whole system prompt (31 KB) on every copy, whatever
//! `logPrompts` says. It is folded: never stored.
//!
//! # Where a span sits
//!
//! Gemini CLI gives every model call and every tool call a trace of its own, and the records of
//! a call carry its context, except the one with its usage (`api_response`). A session's turns
//! would read as dozens of one-span traces. So the spans made here are placed by **turn**
//! instead: each `user_prompt` starts a trace (derived from the session, the prompt id and the
//! prompt's time), remembered for the session ([`crate::logs::Recall`]), and every span after
//! it sits in that trace under the prompt marker. A record's own context is kept as
//! `evald.otel.trace_id` / `evald.otel.span_id`: the model call's names its traced `llm_call`
//! span. With no turn remembered, a span goes to a trace derived from its session and prompt id
//! (Gemini CLI reuses one prompt id for every turn of a headless session, so those merge).
//!
//! # Spans
//!
//! Gemini CLI's spans (`llm_call`, `tool_call`, `schedule_tool_calls`) describe the calls its
//! events describe, and cannot be its only signal: its logs are always on when its telemetry is.
//! They are kept as they arrive and never counted: an `llm_call` span's usage, which lacks the
//! cache and thinking counts, is set aside (`evald.usage = "copy"`), its `tool_call` spans are
//! not read as tool calls (their operation name is Gemini CLI's own), and none is part of a
//! session's rollup (its `session_id` is not set; the attribute stays) ([`fix_trace_span`]).
//! Each also carries Gemini CLI's system prompt and tool list (about 48 KB a model call), and
//! they are sent whatever `telemetry.traces` says (that setting adds prompts and tool output to
//! them): `evald connect gemini-cli` turns them off with `OTEL_TRACES_SAMPLER=always_off`.
//!
//! # Privacy
//!
//! `logPrompts` defaults to **true** in Gemini CLI: `evald connect gemini-cli` turns it off. Its
//! resource attributes include the process's command line (`process.command_args`), which holds
//! the prompt of `gemini -p "…"` whatever `logPrompts` says; evald keeps no resource attribute
//! but `service.name`, so that never reaches the store.

use std::collections::HashMap;
use std::sync::OnceLock;

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::LogRecord;

use crate::logs::{
    derive_id, kv_int, kv_str, record_time, trace_context, Mapping, Recall, RecordCtx, SpanDraft,
};
use crate::model::NormalizedSpan;
use crate::normalize::{attr_str, attr_u64};

/// The `service.name` (and instrumentation scope) Gemini CLI sets.
pub const SERVICE_NAME: &str = "gemini-cli";

const LLM_SPAN: &str = "gemini_cli.llm_request";
const TOOL_SPAN: &str = "gemini_cli.tool";
const PROMPT_SPAN: &str = "gemini_cli.user_prompt";

/// What is remembered about one session between records.
#[derive(Clone, Default)]
pub(crate) struct Session {
    /// The turn in progress.
    turn: Option<Turn>,
    /// The trace context of the model request most recently sent (its `llm_call` span), which
    /// the response that ends it does not carry.
    request: Option<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone)]
struct Turn {
    trace_id: Vec<u8>,
    prompt_span: Vec<u8>,
}

fn recall() -> &'static Recall<Session> {
    static RECALL: OnceLock<Recall<Session>> = OnceLock::new();
    RECALL.get_or_init(Recall::new)
}

fn is_mine(ctx: &RecordCtx) -> bool {
    ctx.service_name.as_deref() == Some(SERVICE_NAME) || ctx.scope_name == Some(SERVICE_NAME)
}

/// Is this span Gemini CLI's? Called once per scope by [`crate::normalize::normalize_request`].
pub(crate) fn is_trace_scope(service_name: Option<&str>) -> bool {
    service_name == Some(SERVICE_NAME)
}

/// Map one Gemini CLI event record ([`crate::logs`] calls this for every record).
pub(crate) fn map_log(
    ctx: &RecordCtx,
    rec: &LogRecord,
    name: Option<&str>,
    local: &mut HashMap<String, Session>,
) -> Mapping {
    if !is_mine(ctx) {
        return Mapping::NotMine;
    }
    let Some(name) = name else {
        return Mapping::NotMine;
    };
    let a = &rec.attributes;
    let session = attr_str(a, "session.id").unwrap_or_default();
    match name {
        "gemini_cli.user_prompt" => {
            let time = record_time(rec);
            let prompt = attr_str(a, "prompt_id").unwrap_or_default();
            let turn = Turn {
                trace_id: derive_id(
                    16,
                    &["gemini_cli.turn", &session, &prompt, &time.to_string()],
                ),
                prompt_span: derive_id(8, &["gemini_cli.prompt", &session, &time.to_string()]),
            };
            let span = SpanDraft {
                trace_id: turn.trace_id.clone(),
                span_id: turn.prompt_span.clone(),
                parent_span_id: Vec::new(),
                name: PROMPT_SPAN.to_string(),
                start_unix_nano: time,
                end_unix_nano: time,
                status: None,
                attributes: common(rec, &session, None),
            }
            .finish(ctx);
            recall().update(local, &session, |s| s.turn = Some(turn));
            Mapping::Span(Box::new(span))
        }
        "gemini_cli.api_response" => {
            Mapping::Span(Box::new(llm_span(ctx, rec, &session, None, local)))
        }
        "gemini_cli.api_error" => {
            let msg = attr_str(a, "error")
                .or_else(|| attr_str(a, "error.message"))
                .or_else(|| attr_str(a, "error_type"))
                .unwrap_or_else(|| "API request failed".into());
            Mapping::Span(Box::new(llm_span(ctx, rec, &session, Some(msg), local)))
        }
        "gemini_cli.tool_call" => Mapping::Span(Box::new(tool_span(ctx, rec, &session, local))),
        "gemini_cli.api_request" => {
            if let (Some(t), Some(sp)) = trace_context(rec) {
                let ctx = (t.to_vec(), sp.to_vec());
                recall().update(local, &session, |s| s.request = Some(ctx));
            }
            Mapping::Folded
        }
        "gen_ai.client.inference.operation.details" => Mapping::Folded,
        _ => Mapping::NotMine,
    }
}

/// Add `kv` unless the record already has that key.
fn add(attrs: &mut Vec<KeyValue>, kv: KeyValue) {
    if !attrs.iter().any(|x| x.key == kv.key) {
        attrs.push(kv);
    }
}

/// The record's attributes, the trace context of the call it describes (`own`: kept, since the
/// span is placed by turn), and what every span made here carries.
fn common(rec: &LogRecord, session: &str, own: Option<(&[u8], &[u8])>) -> Vec<KeyValue> {
    let mut attributes = rec.attributes.clone();
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
    if !session.is_empty() {
        add(&mut attributes, kv_str("gen_ai.conversation.id", session));
    }
    add(&mut attributes, kv_str("evald.source", "log"));
    attributes
}

/// The turn a span belongs to: remembered, else derived from the session and prompt id.
fn placement(
    rec: &LogRecord,
    session: &str,
    local: &HashMap<String, Session>,
) -> (Vec<u8>, Vec<u8>) {
    match recall().get(local, session).and_then(|s| s.turn) {
        Some(t) => (t.trace_id, t.prompt_span),
        None => {
            let prompt = attr_str(&rec.attributes, "prompt_id").unwrap_or_default();
            (
                derive_id(16, &["gemini_cli.turn", session, &prompt]),
                Vec::new(),
            )
        }
    }
}

/// `start = time − duration_ms`: an event is logged when its operation ends.
fn start_of(time: u64, a: &[KeyValue]) -> u64 {
    let ms = attr_u64(a, "duration_ms").unwrap_or(0);
    time.saturating_sub(ms.saturating_mul(1_000_000))
}

fn llm_span(
    ctx: &RecordCtx,
    rec: &LogRecord,
    session: &str,
    error: Option<String>,
    local: &HashMap<String, Session>,
) -> NormalizedSpan {
    let a = &rec.attributes;
    let time = record_time(rec);
    let (trace_id, parent) = placement(rec, session, local);
    // The response carries no context; the request it ends does.
    let remembered = recall().get(local, session).and_then(|s| s.request);
    let own = match trace_context(rec) {
        (Some(t), Some(s)) => Some((t, s)),
        _ => remembered
            .as_ref()
            .map(|(t, s)| (t.as_slice(), s.as_slice())),
    };
    let mut attributes = common(rec, session, own);
    add(&mut attributes, kv_str("gen_ai.operation.name", "chat"));
    let provider = match attr_str(a, "auth_type").as_deref() {
        Some("vertex-ai") => "gcp.vertex_ai",
        _ => "gcp.gemini",
    };
    add(&mut attributes, kv_str("gen_ai.provider.name", provider));
    let model = attr_str(a, "model");
    if let Some(m) = &model {
        add(&mut attributes, kv_str("gen_ai.request.model", m));
        add(&mut attributes, kv_str("gen_ai.response.model", m));
    }
    let input = attr_u64(a, "input_token_count");
    let tool_input = attr_u64(a, "tool_token_count").unwrap_or(0);
    let usage = [
        ("gen_ai.usage.input_tokens", input.map(|i| i + tool_input)),
        (
            "gen_ai.usage.output_tokens",
            attr_u64(a, "output_token_count"),
        ),
        (
            "gen_ai.usage.cache_read.input_tokens",
            attr_u64(a, "cached_content_token_count"),
        ),
        (
            "gen_ai.usage.reasoning.output_tokens",
            attr_u64(a, "thoughts_token_count"),
        ),
        (
            "gen_ai.usage.total_tokens",
            attr_u64(a, "total_token_count"),
        ),
    ];
    for (k, v) in usage {
        if let Some(v) = v {
            add(&mut attributes, kv_int(k, v as i64));
        }
    }
    SpanDraft {
        trace_id,
        span_id: derive_id(
            8,
            &[
                "gemini_cli.llm",
                session,
                &time.to_string(),
                model.as_deref().unwrap_or(""),
            ],
        ),
        parent_span_id: parent,
        name: LLM_SPAN.to_string(),
        start_unix_nano: start_of(time, a),
        end_unix_nano: time,
        status: error.map(Err),
        attributes,
    }
    .finish(ctx)
}

/// The tool a call is about. Gemini CLI names an MCP tool `mcp_<server>_<tool>` and says which
/// server; it is named here as Claude Code and Codex name theirs: `mcp__<server>__<tool>`.
fn tool_name(a: &[KeyValue]) -> Option<String> {
    let name = attr_str(a, "function_name")?;
    let server = attr_str(a, "mcp_server_name").filter(|s| !s.is_empty());
    match server.and_then(|s| {
        name.strip_prefix(&format!("mcp_{s}_"))
            .map(|t| (s.clone(), t))
    }) {
        Some((server, tool)) => Some(format!("mcp__{server}__{tool}")),
        None => Some(name),
    }
}

fn tool_span(
    ctx: &RecordCtx,
    rec: &LogRecord,
    session: &str,
    local: &HashMap<String, Session>,
) -> NormalizedSpan {
    let a = &rec.attributes;
    let time = record_time(rec);
    let (trace_id, parent) = placement(rec, session, local);
    let own = match trace_context(rec) {
        (Some(t), Some(s)) => Some((t, s)),
        _ => None,
    };
    let mut attributes = common(rec, session, own);
    add(
        &mut attributes,
        kv_str("gen_ai.operation.name", "execute_tool"),
    );
    let tool = tool_name(a);
    if let Some(t) = &tool {
        add(&mut attributes, kv_str("gen_ai.tool.name", t));
    }
    // With `logPrompts` on (only), the call's arguments.
    if let Some(args) = attr_str(a, "function_args").filter(|s| !s.is_empty()) {
        add(&mut attributes, kv_str("gen_ai.tool.call.arguments", &args));
    }
    if let Some(server) = attr_str(a, "mcp_server_name").filter(|s| !s.is_empty()) {
        add(&mut attributes, kv_str("mcp.server.name", &server));
    }
    let status = if attr_str(a, "decision").as_deref() == Some("reject") {
        Some(Err("rejected".to_string()))
    } else if attr_str(a, "success").as_deref() == Some("false") {
        Some(Err(
            attr_str(a, "error_type").unwrap_or_else(|| "tool failed".into())
        ))
    } else {
        None
    };
    SpanDraft {
        trace_id,
        span_id: derive_id(
            8,
            &[
                "gemini_cli.tool",
                session,
                &time.to_string(),
                tool.as_deref().unwrap_or(""),
            ],
        ),
        parent_span_id: parent,
        name: TOOL_SPAN.to_string(),
        start_unix_nano: start_of(time, a),
        end_unix_nano: time,
        status,
        attributes,
    }
    .finish(ctx)
}

/// Gemini CLI's spans, as received over traces: an `llm_call` span's usage is set aside, since
/// the `api_response` event reports the same call, with its cache and thinking counts, and no
/// span is part of a session's rollup, whose turns and calls the events give. Called by
/// [`crate::normalize::normalize_request`] for spans of the `gemini-cli` service, before
/// pricing.
pub(crate) fn fix_trace_span(n: &mut NormalizedSpan) {
    if n.name == "llm_call" {
        crate::agent::set_aside(n, crate::agent::USAGE_COPY);
    }
    // Each in a trace of its own: as part of a session they would read as one turn a call.
    n.session_id = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::tests::{logs, traces};
    use crate::logs::{normalize_logs, LogsPolicy, Outcome};

    const SESSION_LOGS: &str =
        include_str!("../tests/fixtures/gemini-cli/0.65.0/session.logs.json");
    const SESSION_SPANS: &str =
        include_str!("../tests/fixtures/gemini-cli/0.65.0/session.traces.json");

    fn by_name<'a>(spans: &'a [NormalizedSpan], name: &str) -> Vec<&'a NormalizedSpan> {
        spans.iter().filter(|s| s.name == name).collect()
    }

    #[test]
    fn every_model_call_is_one_llm_span_with_geminis_token_semantics() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let llm = by_name(&b.spans, LLM_SPAN);
        assert_eq!(llm.len(), 6, "four calls in turn 1, two in turn 2");
        for s in &llm {
            assert_eq!(s.oi_kind.as_deref(), Some("LLM"));
            assert_eq!(s.model.as_deref(), Some("gemini-3.8-flash"));
            assert_eq!(s.provider.as_deref(), Some("gcp.gemini"));
            assert_eq!(
                s.tokens.prompt,
                Some(120),
                "input includes the cached tokens"
            );
            assert_eq!(
                s.tokens.completion,
                Some(24),
                "candidates, without the thinking"
            );
            assert_eq!(s.tokens.reasoning, Some(16), "thinking tokens, on top");
            assert_eq!(s.tokens.total, Some(160));
            assert!(s.cost_usd.is_some_and(|c| c > 0.0), "priced from the table");
            assert_eq!(s.session_id.as_deref(), Some("session-fixture-1"));
            assert!(
                s.end_unix_nano > s.start_unix_nano,
                "duration_ms back to a start"
            );
        }
        assert_eq!(
            llm[0].tokens.cache_read,
            Some(0),
            "a turn's first call is cold"
        );
        assert_eq!(llm[1].tokens.cache_read, Some(64));
        assert!(llm[1].cost_usd < llm[0].cost_usd, "cached input is cheaper");
    }

    #[test]
    fn thinking_tokens_are_priced_once_on_top_of_the_output() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let s = by_name(&b.spans, LLM_SPAN)[0];
        let table = crate::price::current();
        let mut without = s.clone();
        without.tokens.reasoning = None;
        without.tokens.total = Some(144);
        without.cost_usd = None;
        crate::price::apply(&table, &mut without);
        assert!(
            s.cost_usd.unwrap() > without.cost_usd.unwrap(),
            "16 thinking tokens cost something: {:?} vs {:?}",
            s.cost_usd,
            without.cost_usd
        );
    }

    #[test]
    fn a_turn_is_one_trace_under_its_prompt_with_the_calls_own_context_kept() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let prompts = by_name(&b.spans, PROMPT_SPAN);
        assert_eq!(prompts.len(), 2, "two turns");
        assert_ne!(
            prompts[0].trace_id, prompts[1].trace_id,
            "though one prompt id"
        );
        let turns: std::collections::BTreeSet<_> =
            prompts.iter().map(|p| p.trace_id.as_str()).collect();
        for s in b.spans.iter().filter(|s| s.name != PROMPT_SPAN) {
            assert!(
                turns.contains(s.trace_id.as_str()),
                "{} outside a turn",
                s.name
            );
            let prompt = prompts.iter().find(|p| p.trace_id == s.trace_id).unwrap();
            assert_eq!(
                s.parent_span_id.as_deref(),
                Some(prompt.span_id.as_str()),
                "under the turn's prompt"
            );
            assert!(s.raw_attributes.contains_key("evald.otel.span_id"));
        }
        // The model call names the traced `llm_call` span that describes it.
        let spans = crate::normalize::normalize_request(&traces(SESSION_SPANS));
        let first = by_name(&b.spans, LLM_SPAN)[0];
        let traced = first.raw_attributes["evald.otel.span_id"].as_str().unwrap();
        assert!(spans
            .iter()
            .any(|s| s.name == "llm_call" && s.span_id == traced));
    }

    #[test]
    fn tool_calls_are_named_like_the_other_agents_name_them_and_fail_when_they_failed() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let summary: Vec<(String, i32, Option<String>)> = by_name(&b.spans, TOOL_SPAN)
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
                ("run_shell_command".into(), 0, None),
                ("mcp__weather__get_weather".into(), 0, None),
                // Gemini CLI reports `exit 3` as a successful call; its exit code is in the
                // tool's output, which it does not export.
                ("run_shell_command".into(), 0, None),
                (
                    "run_shell_command".into(),
                    2,
                    Some("tool_not_registered".into())
                ),
            ]
        );
    }

    #[test]
    fn the_session_reads_as_two_turns_and_the_semconv_copies_are_never_stored() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai);
        let r = crate::sessions::rollups(&b.spans, false).remove(0);
        assert_eq!(r.trace_count, 2, "two turns");
        assert_eq!((r.llm_calls, r.llm_errors), (6, 0));
        assert_eq!((r.tool_calls, r.tool_errors), (4, 1));
        assert_eq!(r.models, vec!["gemini-3.8-flash".to_string()]);
        assert_eq!(b.outcomes.iter().sum::<u64>(), b.records as u64);
        assert_eq!(b.outcomes[Outcome::Span.index()], (6 + 4 + 2) as u64);
        assert!(
            b.outcomes[Outcome::Folded.index()] >= 6 + 12,
            "api_request and both inference.operation.details copies"
        );
        assert!(b
            .spans
            .iter()
            .all(|s| !s.raw_attributes.contains_key("gen_ai.system_instructions")));
    }

    #[test]
    fn resource_attributes_such_as_the_command_line_are_never_stored() {
        let b = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::All);
        let spans = crate::normalize::normalize_request(&traces(SESSION_SPANS));
        for s in b.spans.iter().chain(&spans) {
            assert!(
                !s.raw_attributes.contains_key("process.command_args"),
                "{}",
                s.name
            );
        }
    }

    #[test]
    fn traced_spans_add_no_model_or_tool_calls() {
        let spans = crate::normalize::normalize_request(&traces(SESSION_SPANS));
        assert_eq!(
            spans
                .iter()
                .filter(|s| s.raw_attributes.contains_key(crate::agent::USAGE_KEY))
                .count(),
            6,
            "every llm_call's usage set aside"
        );
        assert!(spans.iter().all(|s| !s.is_llm_span()));
        assert!(spans.iter().all(|s| !crate::agent::is_kind(s, "TOOL")));
        let mut all = normalize_logs(&logs(SESSION_LOGS), LogsPolicy::Genai).spans;
        all.extend(spans);
        let r = crate::sessions::rollups(&all, false).remove(0);
        assert_eq!(
            (r.llm_calls, r.tool_calls, r.trace_count),
            (6, 4, 2),
            "a trace per turn, not per call"
        );
    }
}
