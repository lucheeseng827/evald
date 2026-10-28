//! MCP: a coding agent asks evald about your own traces, sessions, costs and eval runs.
//!
//! The [Model Context Protocol](https://modelcontextprotocol.io) server, on two transports:
//!
//! * **`POST /mcp` on `evald serve`** (Streamable HTTP, JSON responses, no session state): the
//!   normal setup, since the running server holds the store. Behind the same bearer gate as every
//!   other route. `claude mcp add --transport http evald http://127.0.0.1:4318/mcp`.
//! * **`evald mcp --data-dir <dir>`** on stdio, for a store no server holds (a CI run's store,
//!   a copy, `evald demo`): the store's lock is exclusive, so it cannot share a running server's.
//!
//! **Read-only by default.** The tools are thin layers over what the CLI and the REST reads
//! already do: [`crate::sql`], the session rollup, `evald cost`, `evald latency`,
//! `evald eval compare`. Writing a score needs `--allow-writes` (stdio) or
//! `--mcp-allow-writes` (serve); without it the write tool is not even listed. Nothing here
//! makes an outbound call: an answer comes from the local store and stays on the machine.
//!
//! Long strings in an answer (a captured prompt, a completion) are cut at [`MAX_STRING`]
//! characters, marked, so one trace cannot fill the agent's context; the REST reads and SQL
//! return them whole.

use std::path::Path;

use serde_json::{json, Map, Value as Json};

use crate::{DataType, Score, ScoreSource, ScoreTarget, Store, StoreConfig};

/// Protocol revisions this server speaks, newest first. `initialize` answers with the client's
/// revision when it is one of these, else with the newest.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Longest string kept in a tool result before it is cut.
pub const MAX_STRING: usize = 2_000;

/// What the connected agent may do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Access {
    /// Expose `add_score`.
    pub writes: bool,
}

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Handle one message (or a batch, which the 2025-03-26 revision allows). `None` when nothing
/// is owed in reply: a notification, or a response from the client.
pub async fn handle(store: &Store, access: Access, msg: Json) -> Option<Json> {
    if let Json::Array(batch) = msg {
        if batch.is_empty() {
            return Some(error(Json::Null, INVALID_REQUEST, "empty batch"));
        }
        let mut out = Vec::new();
        for m in batch {
            if let Some(r) = Box::pin(handle(store, access, m)).await {
                out.push(r);
            }
        }
        return (!out.is_empty()).then_some(Json::Array(out));
    }
    let Json::Object(m) = msg else {
        return Some(error(
            Json::Null,
            INVALID_REQUEST,
            "a message is a JSON object",
        ));
    };
    let id = m.get("id").cloned();
    let Some(method) = m.get("method").and_then(Json::as_str) else {
        // A response or an error from the client (to nothing we asked): nothing owed.
        return None;
    };
    // A notification (no id) gets no reply, whatever it is.
    let id = id?;
    let params = m.get("params").cloned().unwrap_or(Json::Null);
    Some(match method {
        "initialize" => ok(id, initialize(&params, access)),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools(access) })),
        "tools/call" => match call(store, access, &params).await {
            Ok(result) => ok(id, result),
            Err(msg) => error(id, INVALID_PARAMS, &msg),
        },
        // No resources or prompts: say so in the shape a client expects, rather than an error.
        "resources/list" => ok(id, json!({ "resources": [] })),
        "prompts/list" => ok(id, json!({ "prompts": [] })),
        other => error(id, METHOD_NOT_FOUND, &format!("method not found: {other}")),
    })
}

/// The JSON-RPC error for a body that is not JSON at all.
pub fn parse_error(detail: &str) -> Json {
    error(Json::Null, PARSE_ERROR, &format!("parse error: {detail}"))
}

fn ok(id: Json, result: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Json, code: i64, message: &str) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Json, access: Access) -> Json {
    let asked = params.get("protocolVersion").and_then(Json::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    let writes = if access.writes {
        " add_score writes a score onto a span, trace or session."
    } else {
        " Read-only."
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "evald", "version": env!("CARGO_PKG_VERSION") },
        "instructions": format!(
            "evald stores this machine's LLM and agent traces, sessions, costs and eval runs. \
             Start with list_sessions or cost_report; sql answers anything else (tables `spans` \
             and `scores`; spans.raw_attributes_json holds every attribute).{writes}"
        ),
    })
}

/// One tool's listing.
fn tool(
    name: &str,
    title: &str,
    description: &str,
    properties: Json,
    required: &[&str],
    read_only: bool,
) -> Json {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
        "annotations": { "readOnlyHint": read_only, "openWorldHint": false },
    })
}

fn tools(access: Access) -> Vec<Json> {
    let limit = json!({ "type": "integer", "minimum": 1, "description": "Most rows to return." });
    let mut out = vec![
        tool(
            "sql",
            "Run a read-only SQL query",
            "DataFusion SQL over the store, read-only. Tables: `spans` (trace_id, span_id, \
             parent_span_id, name, oi_kind, model, provider, prompt_tokens, completion_tokens, \
             total_tokens, cost_usd, input_value, output_value, session_id, user_id, \
             service_name, start_unix_nano, end_unix_nano, status_code, raw_attributes_json) \
             and `scores` (id, target_type, target_id, name, num_value, str_value, source, \
             comment, config_id, ts_unix_nano). scores.target_id joins spans.span_id.",
            json!({ "sql": { "type": "string" }, "limit": limit }),
            &["sql"],
            true,
        ),
        tool(
            "get_trace",
            "Get one trace",
            "Every span of a trace in start order, with the scores attached to it.",
            json!({ "trace_id": { "type": "string", "description": "32 hex characters" } }),
            &["trace_id"],
            true,
        ),
        tool(
            "list_sessions",
            "List recent sessions",
            "The most recently active sessions (an agent's or a chat's run across traces): \
             model and tool calls, failures, cost, steps per trace, suspected tool loops.",
            json!({ "limit": limit }),
            &[],
            true,
        ),
        tool(
            "get_session",
            "Get one session",
            "One session rolled up: duration, traces, model calls, tool calls by tool, tool \
             error rate, repeated identical tool calls, cost, models, agents.",
            json!({ "session_id": { "type": "string" } }),
            &["session_id"],
            true,
        ),
        tool(
            "cost_report",
            "Spend by model, user, session, service or provider",
            "Tokens and USD cost grouped by one dimension, biggest spend first. Spans without a \
             price are counted in missing_cost, never as $0.",
            json!({
                "by": { "type": "string", "enum": ["model", "user", "session", "service", "provider"] },
                "limit": limit,
            }),
            &[],
            true,
        ),
        tool(
            "latency_report",
            "Model-call latency percentiles",
            "Exact p50 / p95 / p99 / max of model-call duration (ms) and time to first token, \
             grouped by model, provider or service.",
            json!({
                "by": { "type": "string", "enum": ["model", "provider", "service"] },
                "limit": limit,
            }),
            &[],
            true,
        ),
        tool(
            "compare_runs",
            "Compare two eval runs",
            "Per evaluator: run A's and run B's mean, the delta (B - A), and where both runs \
             carry statistics a Welch's t-test (p-value, confidence interval on the delta). A \
             negative delta is a regression.",
            json!({
                "run_a": { "type": "string", "description": "baseline run id" },
                "run_b": { "type": "string", "description": "candidate run id" },
                "alpha": { "type": "number", "description": "significance level, default 0.05" },
            }),
            &["run_a", "run_b"],
            true,
        ),
        tool(
            "failing_items",
            "The lowest-scoring items of an eval run",
            "A run's per-item scores below a value (default 1, i.e. anything not perfect), \
             lowest first, each with the span or trace it scored: get_trace shows what happened.",
            json!({
                "run_id": { "type": "string" },
                "evaluator": { "type": "string", "description": "only this evaluator" },
                "below": { "type": "number" },
                "limit": limit,
            }),
            &["run_id"],
            true,
        ),
    ];
    if access.writes {
        out.push(tool(
            "add_score",
            "Score a span, trace or session",
            "Write a score (a number, or a label) onto a span, trace or session, as source `api`.",
            json!({
                "target_type": { "type": "string", "enum": ["span", "trace", "session"] },
                "target_id": { "type": "string" },
                "name": { "type": "string" },
                "value": { "type": "number" },
                "label": { "type": "string" },
                "comment": { "type": "string" },
            }),
            &["target_type", "target_id", "name"],
            false,
        ));
    }
    out
}

/// `tools/call`. `Err` is a protocol error (no such tool); a tool that fails returns
/// `isError: true` with the reason, which the agent can read and act on.
async fn call(store: &Store, access: Access, params: &Json) -> Result<Json, String> {
    let name = params
        .get("name")
        .and_then(Json::as_str)
        .ok_or("tools/call needs a tool name")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let known = tools(access)
        .iter()
        .any(|t| t["name"].as_str() == Some(name));
    if !known {
        return Err(format!("unknown tool: {name}"));
    }
    Ok(match run(store, name, &args).await {
        Ok(value) => {
            let value = trim(value);
            let structured = match value {
                Json::Object(_) => value,
                other => json!({ "result": other }),
            };
            json!({
                "content": [{ "type": "text", "text": structured.to_string() }],
                "structuredContent": structured,
                "isError": false,
            })
        }
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("{e:#}") }],
            "isError": true,
        }),
    })
}

fn arg_str<'a>(args: &'a Json, key: &str) -> anyhow::Result<&'a str> {
    args.get(key)
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`{key}` is required"))
}

fn arg_limit(args: &Json, default: usize, max: usize) -> usize {
    args.get("limit")
        .and_then(Json::as_u64)
        .map_or(default, |n| n as usize)
        .clamp(1, max)
}

/// A string literal for SQL built here (single quotes doubled).
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

async fn run(store: &Store, name: &str, args: &Json) -> anyhow::Result<Json> {
    match name {
        "sql" => {
            let sql = arg_str(args, "sql")?;
            let r = crate::sql::query(store, sql, arg_limit(args, 200, 10_000)).await?;
            Ok(serde_json::to_value(r)?)
        }
        "get_trace" => {
            let trace_id = arg_str(args, "trace_id")?;
            let spans = store.trace(trace_id)?;
            anyhow::ensure!(!spans.is_empty(), "no spans for trace {trace_id}");
            let scores = store.trace_scores(trace_id)?;
            Ok(json!({ "trace_id": trace_id, "spans": spans, "scores": scores }))
        }
        "list_sessions" => {
            let spans = store.query(None, crate::sessions::SCAN_CAP)?;
            let truncated = spans.len() >= crate::sessions::SCAN_CAP;
            let mut rows = crate::sessions::rollups(&spans, truncated);
            rows.truncate(arg_limit(args, 20, 1_000));
            Ok(json!({ "sessions": rows }))
        }
        "get_session" => {
            let id = arg_str(args, "session_id")?;
            let spans = store.query(None, crate::sessions::SCAN_CAP)?;
            let truncated = spans.len() >= crate::sessions::SCAN_CAP;
            let r = crate::sessions::rollup(&spans, id, truncated)
                .ok_or_else(|| anyhow::anyhow!("no spans carry session id {id}"))?;
            Ok(serde_json::to_value(r)?)
        }
        "cost_report" => {
            let by = crate::cost::Dimension::parse(
                args.get("by").and_then(Json::as_str).unwrap_or("model"),
            )?;
            let rows = crate::cost::cost_rows(store, by, arg_limit(args, 20, 1_000)).await?;
            Ok(json!({ "rows": rows }))
        }
        "latency_report" => {
            let by = crate::latency::Dimension::parse(
                args.get("by").and_then(Json::as_str).unwrap_or("model"),
            )?;
            let rows = crate::latency::latency_rows(store, by, arg_limit(args, 20, 1_000)).await?;
            Ok(json!({ "rows": rows }))
        }
        "compare_runs" => {
            let alpha = args.get("alpha").and_then(Json::as_f64).unwrap_or(0.05);
            anyhow::ensure!(alpha > 0.0 && alpha < 1.0, "`alpha` is between 0 and 1");
            let r = crate::eval::compare_runs(
                store,
                arg_str(args, "run_a")?,
                arg_str(args, "run_b")?,
                alpha,
            )?;
            let rows: Vec<Json> = r
                .rows
                .iter()
                .map(|row| {
                    json!({
                        "evaluator": row.evaluator,
                        "a": row.a,
                        "b": row.b,
                        "delta": row.delta,
                        "n_a": row.n_a,
                        "n_b": row.n_b,
                        "p_value": row.signif.map(|s| s.p_two_sided),
                        "ci_low": row.signif.map(|s| s.ci_low),
                        "ci_high": row.signif.map(|s| s.ci_high),
                    })
                })
                .collect();
            Ok(json!({ "run_a": r.run_a, "run_b": r.run_b, "alpha": alpha, "rows": rows }))
        }
        "failing_items" => {
            let run = arg_str(args, "run_id")?;
            let below = args.get("below").and_then(Json::as_f64).unwrap_or(1.0);
            anyhow::ensure!(below.is_finite(), "`below` is a number");
            let evaluator = match args.get("evaluator").and_then(Json::as_str) {
                Some(e) => format!(" AND name = {}", sql_str(e)),
                None => String::new(),
            };
            // A run's per-item scores carry its id as config_id; its aggregates are `<run>:agg:*`.
            let sql = format!(
                "SELECT name AS evaluator, num_value AS value, target_type, target_id, comment, id \
                 FROM scores WHERE config_id = {run_lit} AND NOT starts_with(id, {agg}) \
                 AND num_value < {below}{evaluator} ORDER BY num_value ASC, id",
                run_lit = sql_str(run),
                agg = sql_str(&format!("{run}:agg:")),
            );
            let r = crate::sql::query(store, &sql, arg_limit(args, 50, 10_000)).await?;
            Ok(json!({ "run_id": run, "below": below, "items": r.rows, "truncated": r.truncated }))
        }
        "add_score" => add_score(store, args),
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

fn add_score(store: &Store, args: &Json) -> anyhow::Result<Json> {
    let id = arg_str(args, "target_id")?.to_string();
    let target = match arg_str(args, "target_type")? {
        "span" => ScoreTarget::Span(id),
        "trace" => ScoreTarget::Trace(id),
        "session" => ScoreTarget::Session(id),
        other => anyhow::bail!("`target_type` is span, trace or session, not {other}"),
    };
    let name = arg_str(args, "name")?.to_string();
    let num_value = args.get("value").and_then(Json::as_f64);
    let str_value = args.get("label").and_then(Json::as_str).map(str::to_string);
    anyhow::ensure!(
        num_value.is_some() != str_value.is_some(),
        "give `value` (a number) or `label` (a string), one of them"
    );
    let ts_unix_nano = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let score = Score {
        id: format!("mcp:{}:{name}:{ts_unix_nano}", target.key()),
        target,
        name,
        data_type: if num_value.is_some() {
            DataType::Numeric
        } else {
            DataType::Categorical
        },
        num_value,
        str_value,
        source: ScoreSource::Api,
        comment: args
            .get("comment")
            .and_then(Json::as_str)
            .map(str::to_string),
        config_id: None,
        agg_stats: None,
        ts_unix_nano,
    };
    store.put_scores(std::slice::from_ref(&score))?;
    Ok(json!({ "stored": score }))
}

/// Cut every string longer than [`MAX_STRING`] characters, saying how much was cut.
fn trim(v: Json) -> Json {
    match v {
        Json::String(s) if s.chars().count() > MAX_STRING => {
            let total = s.chars().count();
            let kept: String = s.chars().take(MAX_STRING).collect();
            Json::String(format!(
                "{kept}… [{} more characters; SQL returns the whole value]",
                total - MAX_STRING
            ))
        }
        Json::Array(a) => Json::Array(a.into_iter().map(trim).collect()),
        Json::Object(o) => Json::Object(
            o.into_iter()
                .map(|(k, v)| (k, trim(v)))
                .collect::<Map<_, _>>(),
        ),
        other => other,
    }
}

/// `evald mcp`: the server on stdio, over the store at `data_dir`. One JSON-RPC message per
/// line in, one per line out; logs go to stderr, so stdout carries nothing but the protocol.
pub async fn stdio_command(data_dir: &Path, access: Access) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "opening store at {}: {e} (a running `evald serve` holds its store: point the agent at \
             its POST /mcp instead)",
            data_dir.display()
        )
    })?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Json>(&line) {
            Ok(msg) => handle(&store, access, msg).await,
            Err(e) => Some(parse_error(&e.to_string())),
        };
        if let Some(reply) = reply {
            let mut out = reply.to_string();
            out.push('\n');
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn demo_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        let (spans, scores) = crate::demo::build(1_900_000_000_000_000_000).unwrap();
        store.ingest_batch(spans, &scores).await.unwrap();
        (store, dir)
    }

    async fn rpc(store: &Store, access: Access, method: &str, params: Json) -> Json {
        handle(
            store,
            access,
            json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
        )
        .await
        .expect("a request gets a reply")
    }

    async fn tool_call(store: &Store, access: Access, name: &str, arguments: Json) -> Json {
        rpc(
            store,
            access,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await["result"]
            .clone()
    }

    #[tokio::test]
    async fn initialize_lists_read_only_tools_and_negotiates_the_revision() {
        let (store, _d) = demo_store().await;
        let r = rpc(
            &store,
            Access::default(),
            "initialize",
            json!({ "protocolVersion": "2025-03-26" }),
        )
        .await;
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "evald");
        let r = rpc(
            &store,
            Access::default(),
            "initialize",
            json!({ "protocolVersion": "1999-01-01" }),
        )
        .await;
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);

        let tools = rpc(&store, Access::default(), "tools/list", json!({})).await["result"]
            ["tools"]
            .clone();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "sql",
                "get_trace",
                "list_sessions",
                "get_session",
                "cost_report",
                "latency_report",
                "compare_runs",
                "failing_items"
            ]
        );
        assert!(tools
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == true));
        let with_writes = rpc(&store, Access { writes: true }, "tools/list", json!({})).await;
        assert!(with_writes["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "add_score"));
    }

    #[tokio::test]
    async fn notifications_get_no_reply_and_unknown_methods_an_error() {
        let (store, _d) = demo_store().await;
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert_eq!(handle(&store, Access::default(), note).await, None);
        let r = rpc(&store, Access::default(), "no/such", json!({})).await;
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(r["id"], 1);
        // A batch answers each request in it, and nothing for its notifications.
        let batch = json!([
            { "jsonrpc": "2.0", "id": 7, "method": "ping" },
            { "jsonrpc": "2.0", "method": "notifications/initialized" },
        ]);
        let r = handle(&store, Access::default(), batch).await.unwrap();
        assert_eq!(r.as_array().unwrap().len(), 1);
        assert_eq!(r[0]["id"], 7);
    }

    #[tokio::test]
    async fn the_read_tools_answer_from_the_store() {
        let (store, _d) = demo_store().await;
        let a = Access::default();
        let sessions = tool_call(&store, a, "list_sessions", json!({})).await;
        assert_eq!(sessions["isError"], false);
        let list = sessions["structuredContent"]["sessions"]
            .as_array()
            .unwrap();
        assert_eq!(
            list.len(),
            3,
            "the demo's Claude Code session and two app sessions"
        );
        let id = list
            .iter()
            .find(|s| s["tool_calls"].as_u64() > Some(0))
            .unwrap()["session_id"]
            .clone();
        let one = tool_call(&store, a, "get_session", json!({ "session_id": id })).await;
        assert!(one["structuredContent"]["tools"]
            .as_object()
            .unwrap()
            .contains_key("Bash"));

        let cost = tool_call(&store, a, "cost_report", json!({ "by": "model" })).await;
        let rows = cost["structuredContent"]["rows"].as_array().unwrap();
        assert!(rows.iter().any(|r| r["attribution"] == "claude-sonnet-5"));
        let lat = tool_call(&store, a, "latency_report", json!({})).await;
        assert!(!lat["structuredContent"]["rows"]
            .as_array()
            .unwrap()
            .is_empty());

        let sql = tool_call(&store, a, "sql", json!({ "sql": "SELECT trace_id FROM spans WHERE service_name = 'support-bot' LIMIT 1" })).await;
        let trace = sql["structuredContent"]["rows"][0]["trace_id"]
            .as_str()
            .unwrap()
            .to_string();
        let t = tool_call(&store, a, "get_trace", json!({ "trace_id": trace })).await;
        assert_eq!(t["structuredContent"]["spans"].as_array().unwrap().len(), 3);
        assert!(
            !t["structuredContent"]["scores"]
                .as_array()
                .unwrap()
                .is_empty(),
            "the faithfulness score"
        );
        // The text content is the same answer, for clients that read only text.
        let text: Json = serde_json::from_str(t["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, t["structuredContent"]);
    }

    #[tokio::test]
    async fn a_tool_that_fails_says_why_and_writes_are_refused_unless_allowed() {
        let (store, _d) = demo_store().await;
        let a = Access::default();
        let bad = tool_call(&store, a, "sql", json!({ "sql": "DELETE FROM spans" })).await;
        assert_eq!(bad["isError"], true);
        assert!(
            bad["content"][0]["text"].as_str().unwrap().contains("read"),
            "{bad}"
        );
        let missing = tool_call(
            &store,
            a,
            "get_trace",
            json!({ "trace_id": "00".repeat(16) }),
        )
        .await;
        assert_eq!(missing["isError"], true);

        let score =
            json!({ "target_type": "session", "target_id": "s1", "name": "helpful", "value": 1.0 });
        let refused = rpc(
            &store,
            a,
            "tools/call",
            json!({ "name": "add_score", "arguments": score }),
        )
        .await;
        assert_eq!(
            refused["error"]["code"], INVALID_PARAMS,
            "not listed, not callable"
        );
        assert!(store
            .scores_for_target(&ScoreTarget::Session("s1".into()))
            .unwrap()
            .is_empty());

        let w = Access { writes: true };
        let stored = tool_call(&store, w, "add_score", score).await;
        assert_eq!(stored["isError"], false, "{stored}");
        assert_eq!(
            store
                .scores_for_target(&ScoreTarget::Session("s1".into()))
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn compare_runs_and_failing_items_read_eval_runs() {
        let (store, _d) = demo_store().await;
        let item = |run: &str, name: &str, idx: usize, v: f64| Score {
            id: format!("{run}:{name}:{idx}"),
            target: ScoreTarget::Span(format!("{idx:016x}")),
            name: name.into(),
            num_value: Some(v),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Eval,
            comment: None,
            config_id: Some(run.into()),
            agg_stats: None,
            ts_unix_nano: 1,
        };
        let agg = |run: &str, mean: f64| Score {
            id: format!("{run}:agg:exact_match"),
            target: ScoreTarget::Run(run.into()),
            ..item(run, "exact_match", 0, mean)
        };
        store
            .put_scores(&[
                item("run_a", "exact_match", 0, 1.0),
                item("run_a", "exact_match", 1, 1.0),
                item("run_b", "exact_match", 0, 1.0),
                item("run_b", "exact_match", 1, 0.0),
                agg("run_a", 1.0),
                agg("run_b", 0.5),
            ])
            .unwrap();
        let a = Access::default();
        let cmp = tool_call(
            &store,
            a,
            "compare_runs",
            json!({ "run_a": "run_a", "run_b": "run_b" }),
        )
        .await;
        assert_eq!(cmp["structuredContent"]["rows"][0]["delta"], -0.5);
        let failing = tool_call(&store, a, "failing_items", json!({ "run_id": "run_b" })).await;
        let items = failing["structuredContent"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{failing}");
        assert_eq!(items[0]["target_id"], format!("{:016x}", 1));
        let none = tool_call(&store, a, "failing_items", json!({ "run_id": "run_a" })).await;
        assert!(none["structuredContent"]["items"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn long_strings_are_cut_and_say_so() {
        let v = trim(json!({ "input_value": "x".repeat(MAX_STRING + 10), "n": 1 }));
        let s = v["input_value"].as_str().unwrap();
        assert!(s.starts_with(&"x".repeat(MAX_STRING)));
        assert!(s.contains("10 more characters"));
        assert_eq!(v["n"], 1);
    }
}
