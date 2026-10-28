//! Sessions: what an agent did, one session at a time (`GET /v1/sessions/{id}`, `evald sessions`).
//!
//! A session is the unit a coding agent or a chat lives in: many traces (one per turn, for
//! Claude Code), hours or days long. The rollup answers what an operator asks about one: how
//! long it ran, how many model calls and tool calls it made, how many of each failed, what it
//! cost, and which models and tools it used.
//!
//! **Tool calls are counted once per call**, by `gen_ai.tool.call.id` (or an alias,
//! [`crate::agent`]) when the span carries one. A tool reported on two signals (Claude Code's
//! `tool_result` event and its traced `claude_code.tool` span, [`crate::claude_code`]) is
//! therefore one call here, even where raw SQL shows both copies. A tool span without a call id
//! is its own call, unless its parent is a tool span of the same tool: a call traced at two
//! layers (an agent framework's tool span, and the MCP `tools/call` span under it).
//!
//! **Trajectory**, judge-free: model calls per trace (how many steps a turn took), the
//! tool-error rate, and repeats — a tool call identical to the one just before it in the same
//! trace (same tool, same arguments). [`LOOP_RUN`] identical calls in a row is a suspected loop.
//! A repeat needs the arguments on both calls (`gen_ai.tool.call.arguments` or an alias): calls
//! without them are never repeats, so a tool that reports no arguments cannot raise a false
//! alarm. Whether a session *succeeded* is not evald's to guess; join your own outcome to the
//! session id in SQL.
//!
//! The rollup reads the most recent spans (bounded by the caller) and groups them by
//! `session_id`; there is no session index yet, so a session older than the scan is reported as
//! truncated rather than silently partial.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::model::{NormalizedSpan, Tokens};
use crate::{Store, StoreConfig};

/// OTLP `Status.code` for an error.
#[cfg(test)]
const STATUS_ERROR: i32 = 2;

/// This many identical tool calls in a row (same tool, same arguments, one trace) is a suspected
/// loop: an agent retrying what already failed, or re-reading what it already has.
pub const LOOP_RUN: usize = 3;

/// One session, rolled up.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionRollup {
    pub session_id: String,
    pub span_count: usize,
    /// Distinct traces the session's spans belong to (one per turn, for Claude Code).
    pub trace_count: usize,
    /// Summed `cost_usd` over spans that carry one; absent when none did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Field-wise token totals (only populated fields are summed / emitted).
    #[serde(skip_serializing_if = "Tokens::is_empty")]
    pub tokens: Tokens,
    /// Earliest span start / latest span end (unix nanos): the session's wall-clock extent.
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    /// LLM spans, and how many of them ended in an error status.
    pub llm_calls: usize,
    pub llm_errors: usize,
    /// Distinct tool calls, and how many failed (an error status on any copy of the call; a
    /// refused tool counts as failed).
    pub tool_calls: usize,
    pub tool_errors: usize,
    /// Models the session called, sorted.
    pub models: Vec<String>,
    /// Tool calls by tool name.
    pub tools: BTreeMap<String, usize>,
    /// Agents that ran in the session (`gen_ai.agent.name` or an alias), sorted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Model calls per trace: how many steps a turn took.
    pub steps_per_trace: f64,
    /// `tool_errors / tool_calls`; absent when the session called no tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_error_rate: Option<f64>,
    /// Tool calls identical to the call just before them in the same trace, the longest run of
    /// identical calls, and whether that run reached [`LOOP_RUN`].
    pub repeated_tool_calls: usize,
    pub longest_repeat_run: usize,
    pub loop_suspected: bool,
    /// Spans scanned store-wide to build this rollup, and whether that scan hit its cap (older
    /// spans of the session may then be missing).
    pub scanned: usize,
    pub truncated: bool,
}

struct Acc {
    span_count: usize,
    traces: HashSet<String>,
    cost: Option<f64>,
    tokens: Tokens,
    start: u64,
    end: u64,
    llm_calls: usize,
    llm_errors: usize,
    /// call identity → the call, merged over its copies
    tools: HashMap<String, ToolCall>,
    models: BTreeSet<String>,
    agents: BTreeSet<String>,
}

/// One tool call, merged over every copy of it (`Acc::add`).
struct ToolCall {
    name: String,
    /// Whether `name` came from a tool-name attribute (else it is the span name).
    named: bool,
    failed: bool,
    /// Where the first copy sits: its trace, span, parent and start, for the nesting merge and
    /// the repeat scan.
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    start: u64,
    arguments: Option<String>,
    /// Keyed by call id rather than by span (a nested copy never merges into another call).
    by_id: bool,
}

impl Default for Acc {
    /// `start` begins at `u64::MAX` so the first span's start is the minimum.
    fn default() -> Self {
        Acc {
            span_count: 0,
            traces: HashSet::new(),
            cost: None,
            tokens: Tokens::default(),
            start: u64::MAX,
            end: 0,
            llm_calls: 0,
            llm_errors: 0,
            tools: HashMap::new(),
            models: BTreeSet::new(),
            agents: BTreeSet::new(),
        }
    }
}

impl Acc {
    fn add(&mut self, s: &NormalizedSpan) {
        self.span_count += 1;
        self.traces.insert(s.trace_id.clone());
        if let Some(c) = s.cost_usd {
            self.cost = Some(self.cost.unwrap_or(0.0) + c);
        }
        add_tokens(&mut self.tokens, &s.tokens);
        self.start = self.start.min(s.start_unix_nano);
        self.end = self.end.max(s.end_unix_nano);
        let failed = crate::agent::failed(s);
        if let Some(a) = crate::agent::agent_name(s) {
            if !self.agents.contains(a) {
                self.agents.insert(a.to_string());
            }
        }
        if crate::agent::is_kind(s, "LLM") {
            self.llm_calls += 1;
            self.llm_errors += usize::from(s.status_code == 2);
            if let Some(m) = &s.model {
                self.models.insert(m.clone());
            }
        } else if crate::agent::is_kind(s, "TOOL") {
            let id = crate::agent::tool_call_id(s);
            let call = id
                .map(|id| format!("id:{id}"))
                .unwrap_or_else(|| format!("span:{}:{}", s.trace_id, s.span_id));
            let named = crate::agent::tool_name(s);
            let entry = self.tools.entry(call).or_insert_with(|| ToolCall {
                name: s.name.clone(),
                named: false,
                failed: false,
                trace_id: s.trace_id.clone(),
                span_id: s.span_id.clone(),
                parent_span_id: s.parent_span_id.clone(),
                start: s.start_unix_nano,
                arguments: None,
                by_id: id.is_some(),
            });
            // A copy that names its tool beats one known only by its span name (a phase
            // span of the call, say), whichever arrived first.
            if let (Some(n), false) = (named, entry.named) {
                entry.name = n.to_string();
                entry.named = true;
            }
            if entry.arguments.is_none() {
                entry.arguments = crate::agent::tool_arguments(s);
            }
            entry.start = entry.start.min(s.start_unix_nano);
            entry.failed |= failed;
        }
    }

    fn finish(self, session_id: String, scanned: usize, truncated: bool) -> SessionRollup {
        let calls = merge_nested(self.tools);
        let mut tools = BTreeMap::new();
        let mut tool_errors = 0;
        for c in &calls {
            *tools.entry(c.name.clone()).or_insert(0) += 1;
            tool_errors += usize::from(c.failed);
        }
        let (repeated_tool_calls, longest_repeat_run) = repeats(&calls);
        let trace_count = self.traces.len();
        SessionRollup {
            session_id,
            span_count: self.span_count,
            trace_count,
            cost_usd: self.cost,
            tokens: self.tokens,
            start_unix_nano: self.start,
            end_unix_nano: self.end,
            llm_calls: self.llm_calls,
            llm_errors: self.llm_errors,
            tool_calls: calls.len(),
            tool_errors,
            models: self.models.into_iter().collect(),
            tools,
            agents: self.agents.into_iter().collect(),
            steps_per_trace: if trace_count == 0 {
                0.0
            } else {
                self.llm_calls as f64 / trace_count as f64
            },
            tool_error_rate: (!calls.is_empty()).then(|| tool_errors as f64 / calls.len() as f64),
            repeated_tool_calls,
            longest_repeat_run,
            loop_suspected: longest_repeat_run >= LOOP_RUN,
            scanned,
            truncated,
        }
    }
}

/// Fold a tool span without a call id into its parent when the parent is a tool span of the
/// same tool: one call traced at two layers. The merged call failed if either copy did.
fn merge_nested(tools: HashMap<String, ToolCall>) -> Vec<ToolCall> {
    let mut calls: Vec<ToolCall> = tools.into_values().collect();
    // Children before nothing in particular: resolve each to its outermost same-named ancestor.
    let index: HashMap<(String, String), usize> = calls
        .iter()
        .enumerate()
        .map(|(i, c)| ((c.trace_id.clone(), c.span_id.clone()), i))
        .collect();
    let mut into: Vec<Option<usize>> = vec![None; calls.len()];
    for (i, c) in calls.iter().enumerate() {
        if c.by_id {
            continue;
        }
        let mut at = i;
        // Bounded by the number of calls: a parent chain cannot be longer.
        for _ in 0..calls.len() {
            let Some(parent) = calls[at].parent_span_id.as_ref() else {
                break;
            };
            match index.get(&(calls[at].trace_id.clone(), parent.clone())) {
                Some(&p) if calls[p].name == c.name && p != i => at = p,
                _ => break,
            }
        }
        if at != i {
            into[i] = Some(at);
        }
    }
    let mut failed_into = vec![false; calls.len()];
    for (i, target) in into.iter().enumerate() {
        if let Some(t) = target {
            failed_into[*t] |= calls[i].failed;
        }
    }
    let mut out = Vec::with_capacity(calls.len());
    for (i, mut c) in calls.drain(..).enumerate() {
        if into[i].is_none() {
            c.failed |= failed_into[i];
            out.push(c);
        }
    }
    out
}

/// Tool calls identical to the previous call in the same trace, and the longest run of them.
fn repeats(calls: &[ToolCall]) -> (usize, usize) {
    let mut ordered: Vec<&ToolCall> = calls.iter().collect();
    ordered.sort_by(|a, b| {
        (a.trace_id.as_str(), a.start, a.span_id.as_str()).cmp(&(
            b.trace_id.as_str(),
            b.start,
            b.span_id.as_str(),
        ))
    });
    let (mut repeated, mut longest, mut run) = (0, 0, 0);
    for (i, c) in ordered.iter().enumerate() {
        let same = i > 0 && {
            let p = ordered[i - 1];
            p.trace_id == c.trace_id
                && p.name == c.name
                && c.arguments.is_some()
                && p.arguments == c.arguments
        };
        if same {
            repeated += 1;
            run += 1;
        } else {
            run = 1;
        }
        longest = longest.max(if c.arguments.is_some() { run } else { 0 });
    }
    (repeated, longest)
}

/// Fold one span's populated token fields into the running per-field totals.
pub(crate) fn add_tokens(acc: &mut Tokens, t: &Tokens) {
    fn add(acc: &mut Option<u64>, v: Option<u64>) {
        if let Some(v) = v {
            *acc = Some(acc.unwrap_or(0) + v);
        }
    }
    add(&mut acc.prompt, t.prompt);
    add(&mut acc.completion, t.completion);
    add(&mut acc.total, t.total);
    add(&mut acc.cache_read, t.cache_read);
    add(&mut acc.cache_write, t.cache_write);
    add(&mut acc.reasoning, t.reasoning);
}

/// One session's rollup from `spans` (a scan of `scanned` spans that hit its cap when
/// `truncated`), or `None` when no span carries that session id.
pub fn rollup(
    spans: &[NormalizedSpan],
    session_id: &str,
    truncated: bool,
) -> Option<SessionRollup> {
    let mut acc = Acc::default();
    for s in spans {
        if s.session_id.as_deref() == Some(session_id) {
            acc.add(s);
        }
    }
    (acc.span_count > 0).then(|| acc.finish(session_id.to_string(), spans.len(), truncated))
}

/// Every session in `spans`, most recently active first.
pub fn rollups(spans: &[NormalizedSpan], truncated: bool) -> Vec<SessionRollup> {
    let mut by_session: HashMap<&str, Acc> = HashMap::new();
    for s in spans {
        if let Some(id) = s.session_id.as_deref() {
            by_session.entry(id).or_default().add(s);
        }
    }
    let mut out: Vec<SessionRollup> = by_session
        .into_iter()
        .map(|(id, acc)| acc.finish(id.to_string(), spans.len(), truncated))
        .collect();
    out.sort_by(|a, b| {
        b.end_unix_nano
            .cmp(&a.end_unix_nano)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    out
}

/// How many of the most recent spans a session rollup reads.
pub const SCAN_CAP: usize = 50_000;

/// `evald sessions`: the most recently active sessions in the store at `data_dir`.
pub async fn sessions_command(data_dir: &Path, limit: usize, json: bool) -> anyhow::Result<()> {
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let spans = store.query(None, SCAN_CAP)?;
    let truncated = spans.len() >= SCAN_CAP;
    let mut rows = rollups(&spans, truncated);
    rows.truncate(limit);
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_report(&rows, truncated);
    }
    Ok(())
}

fn print_report(rows: &[SessionRollup], truncated: bool) {
    println!("\nevald sessions — most recently active first");
    println!(
        "{:<36} {:>20} {:>9} {:>6} {:>5} {:>4} {:>5} {:>5} {:>6} {:>4} {:>11}  models",
        "session",
        "started (UTC)",
        "duration",
        "traces",
        "llm",
        "err",
        "tools",
        "t-err",
        "steps",
        "loop",
        "cost $"
    );
    for r in rows {
        let started = chrono::DateTime::from_timestamp_nanos(r.start_unix_nano as i64)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let secs = r.end_unix_nano.saturating_sub(r.start_unix_nano) as f64 / 1e9;
        let duration = if secs >= 3600.0 {
            format!("{:.1}h", secs / 3600.0)
        } else if secs >= 60.0 {
            format!("{:.1}m", secs / 60.0)
        } else {
            format!("{secs:.1}s")
        };
        let cost = r
            .cost_usd
            .map_or_else(|| "(no price)".to_string(), |c| format!("{c:.6}"));
        println!(
            "{:<36} {:>20} {:>9} {:>6} {:>5} {:>4} {:>5} {:>5} {:>6.1} {:>4} {:>11}  {}",
            r.session_id,
            started,
            duration,
            r.trace_count,
            r.llm_calls,
            r.llm_errors,
            r.tool_calls,
            r.tool_errors,
            r.steps_per_trace,
            if r.loop_suspected { "yes" } else { "-" },
            cost,
            r.models.join(",")
        );
    }
    if rows.is_empty() {
        println!("(no spans carry a session id)");
    }
    if rows.iter().any(|r| r.loop_suspected) {
        println!(
            "\nloop: {LOOP_RUN} or more identical tool calls in a row (same tool, same arguments) \
             in one trace; `evald sessions --json` has the counts"
        );
    }
    if truncated {
        println!(
            "\nnote: only the most recent {SCAN_CAP} spans were read; older sessions may be \
             missing or partial"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::tests::{logs, traces, EVENTS_ONLY, TRACED_LOGS, TRACED_SPANS};
    use crate::logs::{normalize_logs, LogsPolicy};

    #[test]
    fn a_claude_code_session_reads_as_calls_tools_cost_and_models() {
        let spans = normalize_logs(&logs(EVENTS_ONLY), LogsPolicy::Genai).spans;
        let all = rollups(&spans, false);
        assert_eq!(all.len(), 1);
        let r = &all[0];
        assert_eq!(r.llm_calls, 2);
        assert_eq!(r.llm_errors, 0);
        assert_eq!(r.tool_calls, 2);
        assert_eq!(r.tools.get("Bash"), Some(&1));
        assert_eq!(r.tools.get("Write"), Some(&1));
        assert_eq!(r.models, vec!["claude-haiku-5-5".to_string()]);
        let cost = r.cost_usd.unwrap();
        assert!((cost - (0.00238249 + 0.00047599)).abs() < 1e-12, "{cost}");
        assert_eq!(r.trace_count, 1, "one turn");
        assert!(r.end_unix_nano > r.start_unix_nano);
        assert_eq!(rollup(&spans, &r.session_id, false).as_ref(), Some(r));
        assert_eq!(rollup(&spans, "nope", false), None);
    }

    #[test]
    fn a_tool_on_both_signals_is_one_call() {
        // Both copies of the Bash call: the tool_result event and the traced tool span (and the
        // traced execution span under it, which is a phase of the call, not another call).
        let mut spans = normalize_logs(&logs(TRACED_LOGS), LogsPolicy::Genai).spans;
        spans.extend(crate::normalize::normalize_request(&traces(TRACED_SPANS)));
        let all = rollups(&spans, false);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].tool_calls, 1, "{:?}", all[0].tools);
        assert_eq!(all[0].tools.get("Bash"), Some(&1));
    }

    #[test]
    fn a_call_is_named_by_the_copy_that_names_its_tool() {
        let spans = normalize_logs(&logs(EVENTS_ONLY), LogsPolicy::Genai).spans;
        let tool = spans
            .iter()
            .find(|s| s.oi_kind.as_deref() == Some("TOOL"))
            .unwrap();
        // A phase span of the same call, classified TOOL by its call id alone and seen first.
        let mut phase = tool.clone();
        phase.name = "claude_code.tool.execution".into();
        phase.span_id = "eeeeeeeeeeeeeeee".into();
        phase.raw_attributes.remove("gen_ai.tool.name");
        phase.raw_attributes.remove("tool_name");
        let mut ordered = vec![phase];
        ordered.extend(spans.iter().cloned());
        let r = &rollups(&ordered, false)[0];
        assert_eq!(r.tool_calls, 2);
        assert!(
            !r.tools.contains_key("claude_code.tool.execution"),
            "{:?}",
            r.tools
        );
    }

    #[test]
    fn a_failed_tool_counts_once_even_if_only_one_copy_failed() {
        let mut spans = normalize_logs(&logs(EVENTS_ONLY), LogsPolicy::Genai).spans;
        let mut twin = spans
            .iter()
            .find(|s| s.oi_kind.as_deref() == Some("TOOL"))
            .unwrap()
            .clone();
        twin.span_id = "ffffffffffffffff".into();
        twin.status_code = STATUS_ERROR;
        spans.push(twin);
        let r = &rollups(&spans, false)[0];
        assert_eq!(r.tool_calls, 2);
        assert_eq!(r.tool_errors, 1);
    }

    /// A tool span of an agent framework's, as the recipes' frameworks emit them.
    fn tool(
        trace: &str,
        span: &str,
        parent: Option<&str>,
        start: u64,
        name: &str,
        args: Option<&str>,
    ) -> NormalizedSpan {
        let mut t = normalize_logs(&logs(EVENTS_ONLY), LogsPolicy::Genai)
            .spans
            .into_iter()
            .find(|s| s.oi_kind.as_deref() == Some("TOOL"))
            .unwrap();
        t.session_id = Some("s".into());
        t.trace_id = trace.into();
        t.span_id = span.into();
        t.parent_span_id = parent.map(Into::into);
        t.start_unix_nano = start;
        t.end_unix_nano = start + 1;
        t.status_code = 0;
        t.raw_attributes.clear();
        t.raw_attributes
            .insert("gen_ai.tool.name".into(), name.into());
        if let Some(a) = args {
            t.raw_attributes
                .insert("gen_ai.tool.call.arguments".into(), a.into());
        }
        t
    }

    #[test]
    fn three_identical_calls_in_a_row_are_a_suspected_loop() {
        let paris = Some(r#"{"city":"Paris"}"#);
        let spans = vec![
            tool("t1", "a", None, 1, "get_weather", paris),
            tool("t1", "b", None, 2, "get_weather", paris),
            tool("t1", "c", None, 3, "get_weather", paris),
            tool(
                "t1",
                "d",
                None,
                4,
                "get_weather",
                Some(r#"{"city":"Rome"}"#),
            ),
        ];
        let r = &rollups(&spans, false)[0];
        assert_eq!(r.tool_calls, 4);
        assert_eq!(r.repeated_tool_calls, 2);
        assert_eq!(r.longest_repeat_run, 3);
        assert!(r.loop_suspected);

        // Two in a row is a retry, not a loop; and the same call in another trace is another turn.
        let spans = vec![
            tool("t1", "a", None, 1, "get_weather", paris),
            tool("t1", "b", None, 2, "get_weather", paris),
            tool("t2", "c", None, 3, "get_weather", paris),
        ];
        let r = &rollups(&spans, false)[0];
        assert_eq!((r.repeated_tool_calls, r.longest_repeat_run), (1, 2));
        assert!(!r.loop_suspected);
    }

    #[test]
    fn calls_without_arguments_are_never_repeats() {
        // Claude Code reports no arguments unless OTEL_LOG_TOOL_DETAILS=1: reading three
        // different files must not look like a loop.
        let spans: Vec<_> = (0..5)
            .map(|i| tool("t1", &format!("s{i}"), None, i, "Read", None))
            .collect();
        let r = &rollups(&spans, false)[0];
        assert_eq!(r.tool_calls, 5);
        assert_eq!((r.repeated_tool_calls, r.longest_repeat_run), (0, 0));
        assert!(!r.loop_suspected);
    }

    #[test]
    fn a_call_traced_at_two_layers_is_one_call() {
        // The framework's tool span, and the MCP tools/call span under it; the inner one failed.
        let outer = tool("t1", "a", None, 1, "get_weather", None);
        let mut inner = tool("t1", "b", Some("a"), 2, "get_weather", None);
        inner
            .raw_attributes
            .insert("mcp.method.name".into(), "tools/call".into());
        inner
            .raw_attributes
            .insert("error.type".into(), "tool_error".into());
        // A different tool under the same parent is a call of its own.
        let other = tool("t1", "c", Some("a"), 3, "geocode", None);
        let r = &rollups(&[outer, inner, other], false)[0];
        assert_eq!(r.tool_calls, 2, "{:?}", r.tools);
        assert_eq!(r.tools.get("get_weather"), Some(&1));
        assert_eq!(r.tool_errors, 1, "the merged call failed");
        assert_eq!(r.tool_error_rate, Some(0.5));
    }

    #[test]
    fn steps_and_agents_come_from_the_model_calls_and_agent_names() {
        let mut spans = normalize_logs(&logs(EVENTS_ONLY), LogsPolicy::Genai).spans;
        let mut agent = spans[0].clone();
        agent.span_id = "abababababababab".into();
        agent.oi_kind = Some("AGENT".into());
        agent
            .raw_attributes
            .insert("agent.name".into(), "planner".into());
        spans.push(agent);
        let r = &rollups(&spans, false)[0];
        assert_eq!(r.agents, vec!["planner".to_string()]);
        assert_eq!(r.steps_per_trace, 2.0, "two model calls in one turn");
        assert_eq!(r.tool_error_rate, Some(0.0));
    }
}
