//! Agent, tool and MCP attributes, read one way whatever emitted them.
//!
//! The same facts arrive under different names: the OpenTelemetry GenAI conventions say
//! `gen_ai.tool.name`, OpenInference says `tool.name`, the Vercel AI SDK's legacy integration
//! `ai.toolCall.name`, Claude Code's events `tool_name`. The accessors here ([`tool_name`],
//! [`tool_call_id`], [`agent_name`], [`tool_arguments`]) try each spelling, so a report reads
//! spans stored by any release the same way. At ingest, [`normalize`] also writes the canonical
//! `gen_ai.*` key onto a tool or agent span that carries only an alias, so one key answers a SQL
//! query too. The alias stays: `raw_attributes` remains a superset of what was sent.
//!
//! ## Aggregate usage on agent spans
//!
//! The conventions let an `invoke_agent` span report the token usage of the whole invocation,
//! and frameworks do (the Vercel AI SDK, for one). When the agent runs **in process**, its model
//! calls are spans of their own, already counted: summing the agent span too counts every token
//! and every dollar twice. So an AGENT or CHAIN span that is not a `CLIENT` span keeps its usage
//! only in `raw_attributes` (where it arrived), carries no `tokens` and no `cost_usd`, and is
//! marked `evald.usage = "aggregate"`. A `CLIENT` agent span is a call to a **remote** agent
//! (Bedrock Agents, an assistant API): its model calls happen elsewhere and leave no spans here,
//! so its usage is the only record of them and counts.
//!
//! ## MCP
//!
//! An MCP `tools/call` span (`mcp.method.name`) is a TOOL span named by `gen_ai.tool.name`, the
//! conventions' choice. When an agent framework traces the same call too, the MCP span is a
//! child of the framework's tool span with the same tool name, and the session rollup counts the
//! two as one call ([`crate::sessions`]).

use serde_json::Value as Json;

use crate::model::{NormalizedSpan, Tokens};

/// Marks an agent or chain span whose usage was the sum of its children's.
pub const USAGE_KEY: &str = "evald.usage";
pub const USAGE_AGGREGATE: &str = "aggregate";
/// Marks a span whose usage another record of the same call reports, and counts: a coding
/// agent's traced copy of a call its events describe ([`crate::codex`], [`crate::gemini_cli`]).
pub const USAGE_COPY: &str = "copy";

/// The OTLP `SpanKind` for a client span.
const SPAN_KIND_CLIENT: i32 = 3;

/// Spellings of each fact, canonical key first.
const TOOL_NAME: [&str; 4] = [
    "gen_ai.tool.name",
    "tool.name",
    "ai.toolCall.name",
    "tool_name",
];
const TOOL_CALL_ID: [&str; 4] = [
    "gen_ai.tool.call.id",
    "tool_call.id",
    "ai.toolCall.id",
    "tool_use_id",
];
const TOOL_ARGUMENTS: [&str; 4] = [
    "gen_ai.tool.call.arguments",
    "ai.toolCall.args",
    // Claude Code, with OTEL_LOG_TOOL_DETAILS=1.
    "tool_input",
    "input.value",
];
const AGENT_NAME: [&str; 2] = ["gen_ai.agent.name", "agent.name"];
const AGENT_ID: [&str; 2] = ["gen_ai.agent.id", "agent.id"];

fn first_str<'a>(s: &'a NormalizedSpan, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| {
        s.raw_attributes
            .get(*k)
            .and_then(Json::as_str)
            .filter(|v| !v.is_empty())
    })
}

/// Whether the span's kind is `kind` (OpenInference kinds arrive in either case).
pub fn is_kind(s: &NormalizedSpan, kind: &str) -> bool {
    s.oi_kind
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case(kind))
}

/// The tool a TOOL span ran.
pub fn tool_name(s: &NormalizedSpan) -> Option<&str> {
    first_str(s, &TOOL_NAME)
}

/// The id of the tool call, shared by every span that reports the same call.
pub fn tool_call_id(s: &NormalizedSpan) -> Option<&str> {
    first_str(s, &TOOL_CALL_ID)
}

/// The arguments the tool was called with, as sent (a JSON string, usually).
pub fn tool_arguments(s: &NormalizedSpan) -> Option<String> {
    TOOL_ARGUMENTS
        .iter()
        .find_map(|k| match s.raw_attributes.get(*k)? {
            Json::Null => None,
            Json::String(v) if v.is_empty() => None,
            Json::String(v) => Some(v.clone()),
            other => Some(other.to_string()),
        })
}

/// The agent an AGENT span (or any span an agent framework stamped) belongs to.
pub fn agent_name(s: &NormalizedSpan) -> Option<&str> {
    first_str(s, &AGENT_NAME)
}

/// Whether a span reports a failure: an error status, or an `error.type` (the conventions' way,
/// and how an MCP tool result with `isError` is recorded).
pub fn failed(s: &NormalizedSpan) -> bool {
    s.status_code == 2 || s.raw_attributes.contains_key("error.type")
}

/// Ingest-time pass over one normalized span (from [`crate::normalize::normalize_span`], before
/// pricing): canonical keys on tool and agent spans, and aggregate usage set aside.
pub(crate) fn normalize(s: &mut NormalizedSpan) {
    let Some(kind) = s.oi_kind.as_deref() else {
        return;
    };
    if kind.eq_ignore_ascii_case("TOOL") {
        canonical(s, &TOOL_NAME);
        canonical(s, &TOOL_CALL_ID);
    } else if kind.eq_ignore_ascii_case("AGENT") || kind.eq_ignore_ascii_case("CHAIN") {
        canonical(s, &AGENT_NAME);
        canonical(s, &AGENT_ID);
        if s.otel_kind != SPAN_KIND_CLIENT && (!s.tokens.is_empty() || s.cost_usd.is_some()) {
            set_aside(s, USAGE_AGGREGATE);
        }
    }
}

/// Keep `s`'s usage only in `raw_attributes` (where it arrived): no `tokens`, no `cost_usd`, not
/// a model call, and `evald.usage = why`.
pub(crate) fn set_aside(s: &mut NormalizedSpan, why: &str) {
    s.tokens = Tokens::default();
    s.cost_usd = None;
    if s.oi_kind
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case("LLM"))
    {
        s.oi_kind = Some("CHAIN".into());
    }
    s.raw_attributes
        .insert(USAGE_KEY.into(), Json::String(why.into()));
}

/// Copy the first alias present onto the canonical key (`keys[0]`) when that key is absent.
fn canonical(s: &mut NormalizedSpan, keys: &[&str]) {
    if s.raw_attributes.contains_key(keys[0]) {
        return;
    }
    if let Some(v) = keys[1..]
        .iter()
        .find_map(|k| s.raw_attributes.get(*k).filter(|v| v.is_string()).cloned())
    {
        s.raw_attributes.insert(keys[0].into(), v);
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
    fn s(k: &str, v: &str) -> KeyValue {
        kv(k, Value::StringValue(v.into()))
    }
    fn i(k: &str, v: i64) -> KeyValue {
        kv(k, Value::IntValue(v))
    }
    fn span(name: &str, kind: i32, attributes: Vec<KeyValue>) -> NormalizedSpan {
        let span = Span {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            name: name.into(),
            kind,
            start_time_unix_nano: 1,
            end_time_unix_nano: 2,
            attributes,
            ..Default::default()
        };
        normalize_span(&span, Some("svc".into()), None, None)
    }

    /// The Vercel AI SDK (7.x, `@ai-sdk/otel`), as captured by examples/recipes/vercel-ai-sdk:
    /// the in-process agent span reports the sum of its two model calls.
    #[test]
    fn an_in_process_agent_span_does_not_count_its_childrens_usage_again() {
        let mut agent = span(
            "invoke_agent gpt-4o-mini",
            1,
            vec![
                s("gen_ai.operation.name", "invoke_agent"),
                s("gen_ai.agent.name", "weather"),
                s("gen_ai.request.model", "gpt-4o-mini"),
                i("gen_ai.usage.input_tokens", 240),
                i("gen_ai.usage.output_tokens", 48),
            ],
        );
        crate::price::apply(&crate::price::PriceTable::baseline(), &mut agent);
        assert_eq!(agent.oi_kind.as_deref(), Some("AGENT"));
        assert!(agent.tokens.is_empty(), "{:?}", agent.tokens);
        assert_eq!(agent.cost_usd, None, "not priced: its children were");
        assert_eq!(agent.raw_attributes[USAGE_KEY], USAGE_AGGREGATE);
        assert_eq!(agent.raw_attributes["gen_ai.usage.input_tokens"], 240);
        assert!(!agent.is_llm_span(), "an agent span is not a model call");

        // A remote agent's usage is the only record of its model calls.
        let mut remote = span(
            "invoke_agent travel",
            SPAN_KIND_CLIENT,
            vec![
                s("gen_ai.operation.name", "invoke_agent"),
                s("gen_ai.request.model", "gpt-4o-mini"),
                i("gen_ai.usage.input_tokens", 240),
                i("gen_ai.usage.output_tokens", 48),
            ],
        );
        crate::price::apply(&crate::price::PriceTable::baseline(), &mut remote);
        assert_eq!(remote.tokens.total, Some(288));
        assert!(remote.cost_usd.is_some());
        assert!(!remote.raw_attributes.contains_key(USAGE_KEY));
    }

    #[test]
    fn a_step_or_workflow_is_a_chain_not_a_model_call() {
        for op in ["agent_step", "invoke_workflow", "plan", "create_memory"] {
            let step = span("step 1", 1, vec![s("gen_ai.operation.name", op)]);
            assert_eq!(step.oi_kind.as_deref(), Some("CHAIN"), "{op}");
            assert!(!step.is_llm_span(), "{op}");
        }
        let retrieval = span(
            "retrieval",
            1,
            vec![s("gen_ai.operation.name", "retrieval")],
        );
        assert_eq!(retrieval.oi_kind.as_deref(), Some("RETRIEVER"));
        // An operation evald does not know, with a model call's attributes, is a model call.
        let call = span(
            "x",
            3,
            vec![
                s("gen_ai.operation.name", "fetch_response"),
                s("gen_ai.request.model", "gpt-4o-mini"),
            ],
        );
        assert_eq!(call.oi_kind.as_deref(), Some("LLM"));
    }

    #[test]
    fn aliases_get_the_canonical_key_and_keep_their_own() {
        // OpenInference (LangGraph, the OpenAI Agents SDK, CrewAI recipes).
        let tool = span(
            "get_weather",
            1,
            vec![
                s("openinference.span.kind", "TOOL"),
                s("tool.name", "get_weather"),
                s("tool_call.id", "call_1"),
            ],
        );
        assert_eq!(tool.raw_attributes["gen_ai.tool.name"], "get_weather");
        assert_eq!(tool.raw_attributes["gen_ai.tool.call.id"], "call_1");
        assert_eq!(tool.raw_attributes["tool.name"], "get_weather");
        let agent = span(
            "weather-agent",
            1,
            vec![
                s("openinference.span.kind", "AGENT"),
                s("agent.name", "weather-agent"),
            ],
        );
        assert_eq!(agent_name(&agent), Some("weather-agent"));
        assert_eq!(agent.raw_attributes["gen_ai.agent.name"], "weather-agent");
        // A canonical key that was sent is never overwritten.
        let both = span(
            "t",
            1,
            vec![
                s("gen_ai.operation.name", "execute_tool"),
                s("gen_ai.tool.name", "a"),
                s("tool.name", "b"),
            ],
        );
        assert_eq!(tool_name(&both), Some("a"));
    }

    #[test]
    fn an_mcp_tools_call_span_is_a_tool_call() {
        let call = span(
            "tools/call get_weather",
            3,
            vec![
                s("mcp.method.name", "tools/call"),
                s("gen_ai.tool.name", "get_weather"),
                s("mcp.session.id", "191c4850af6c49e08843a3f6c80e5046"),
                s("error.type", "tool_error"),
            ],
        );
        assert_eq!(call.oi_kind.as_deref(), Some("TOOL"));
        assert_eq!(tool_name(&call), Some("get_weather"));
        assert!(failed(&call), "an isError result is a failed call");
        let list = span("tools/list", 3, vec![s("mcp.method.name", "tools/list")]);
        assert_eq!(list.oi_kind, None, "protocol traffic is not a tool call");
    }

    #[test]
    fn tool_arguments_read_any_spelling() {
        let t = span(
            "t",
            1,
            vec![
                s("gen_ai.operation.name", "execute_tool"),
                s("gen_ai.tool.call.arguments", "{\"city\":\"Paris\"}"),
            ],
        );
        assert_eq!(tool_arguments(&t).as_deref(), Some("{\"city\":\"Paris\"}"));
        let oi = span(
            "t",
            1,
            vec![
                s("openinference.span.kind", "TOOL"),
                s("input.value", "{\"city\": \"Paris\"}"),
            ],
        );
        assert_eq!(
            tool_arguments(&oi).as_deref(),
            Some("{\"city\": \"Paris\"}")
        );
    }
}
