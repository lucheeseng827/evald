//! `evald demo`: a store with something in it, so the first look at evald is not an empty UI.
//!
//! It writes into a fresh data dir (it refuses one that already holds data):
//!
//! * a **Claude Code session**: the telemetry captured from Claude Code 2.1.306 that the
//!   dialect's tests use (`tests/fixtures/claude-code/`), two model calls and two tool calls;
//! * three traces from a small **RAG app**, `support-bot`: a chain, a retriever and a model
//!   call each, with token counts priced from the built-in table, one failed call, and a
//!   `gen_ai.evaluation.result` score on every model call.
//!
//! Every timestamp is moved to the last half hour (the session ends 25 minutes ago, the app's
//! newest span a minute ago), so the UI's recent views show it. Nothing is fetched: the data is compiled into the binary.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{
    span::Event, status, ResourceSpans, ScopeSpans, Span, Status,
};

use crate::logs::{kv_f64, kv_int, kv_str, LogsPolicy};
use crate::{NormalizedSpan, Score, Store, StoreConfig};

const CLAUDE_CODE_SESSION: &str =
    include_str!("../tests/fixtures/claude-code/2.1.306/events-only.logs.json");

/// What the demo wrote.
#[derive(Debug)]
pub struct Seeded {
    pub spans: usize,
    pub scores: usize,
    pub sessions: usize,
}

/// Build the demo spans and scores. Each part is moved to the recent past on its own: the Claude
/// Code session ends 25 minutes before `now_ns`, the app's newest span a minute before.
pub fn build(now_ns: u64) -> anyhow::Result<(Vec<NormalizedSpan>, Vec<Score>)> {
    const MIN: u64 = 60_000_000_000;
    let req = crate::ingest::decode_otlp_json_as(CLAUDE_CODE_SESSION.as_bytes())?;
    let mut agent = crate::logs::normalize_logs(&req, LogsPolicy::Genai).spans;
    shift_to(&mut agent, &mut [], now_ns.saturating_sub(25 * MIN));

    let app = app_traces();
    let mut spans = crate::normalize::normalize_request(&app);
    let mut scores = crate::evalevent::extract(&app);
    shift_to(&mut spans, &mut scores, now_ns.saturating_sub(MIN));

    spans.extend(agent);
    Ok((spans, scores))
}

/// Move `spans` (and `scores` with them) so the newest span ends at `end_ns`.
fn shift_to(spans: &mut [NormalizedSpan], scores: &mut [Score], end_ns: u64) {
    let newest = spans.iter().map(|s| s.end_unix_nano).max().unwrap_or(0);
    let shift = end_ns as i128 - newest as i128;
    let moved = |t: u64| (t as i128 + shift).max(0) as u64;
    for s in spans {
        s.start_unix_nano = moved(s.start_unix_nano);
        s.end_unix_nano = moved(s.end_unix_nano);
    }
    for sc in scores {
        sc.ts_unix_nano = moved(sc.ts_unix_nano);
    }
}

/// Three `support-bot` turns: chain → retriever → model call, 10 minutes apart.
fn app_traces() -> ExportTraceServiceRequest {
    // (model, input tokens, output tokens, latency ms, faithfulness, failed)
    let turns = [
        ("claude-sonnet-5", 1_840u64, 312u64, 2_400u64, 0.92, false),
        ("gpt-4o-mini", 1_210, 198, 1_150, 0.61, false),
        ("claude-sonnet-5", 2_030, 0, 30_000, 0.0, true),
    ];
    let base = 1_800_000_000_000_000_000u64; // replaced by the shift in `build`
    let mut spans = Vec::new();
    for (i, (model, input, output, latency_ms, faithfulness, failed)) in
        turns.into_iter().enumerate()
    {
        let trace = [i as u8 + 1; 16].to_vec();
        let (root, retriever, llm) = (
            vec![0x10 + i as u8; 8],
            vec![0x20 + i as u8; 8],
            vec![0x30 + i as u8; 8],
        );
        let start = base + i as u64 * 600_000_000_000;
        let retrieved = start + 180_000_000;
        let end = retrieved + latency_ms * 1_000_000;
        let session = format!("support-session-{}", i / 2 + 1);
        let common = |extra: Vec<KeyValue>| {
            let mut a = vec![
                kv_str("session.id", &session),
                kv_str("user.id", if i == 1 { "user-b" } else { "user-a" }),
            ];
            a.extend(extra);
            a
        };
        spans.push(Span {
            trace_id: trace.clone(),
            span_id: root.clone(),
            name: "answer_question".into(),
            kind: 1,
            start_time_unix_nano: start,
            end_time_unix_nano: end + 20_000_000,
            attributes: common(vec![
                kv_str("openinference.span.kind", "CHAIN"),
                kv_str("input.value", "How do I rotate my API key?"),
            ]),
            ..Default::default()
        });
        spans.push(Span {
            trace_id: trace.clone(),
            span_id: retriever,
            parent_span_id: root.clone(),
            name: "vector_search".into(),
            kind: 1,
            start_time_unix_nano: start + 5_000_000,
            end_time_unix_nano: retrieved,
            attributes: common(vec![
                kv_str("openinference.span.kind", "RETRIEVER"),
                kv_int("retrieval.documents.count", 4),
            ]),
            ..Default::default()
        });
        let provider = if model.starts_with("gpt") {
            "openai"
        } else {
            "anthropic"
        };
        let mut attrs = common(vec![
            kv_str("gen_ai.operation.name", "chat"),
            kv_str("gen_ai.provider.name", provider),
            kv_str("gen_ai.request.model", model),
            kv_int("gen_ai.usage.input_tokens", input as i64),
        ]);
        if output > 0 {
            attrs.push(kv_int("gen_ai.usage.output_tokens", output as i64));
        }
        let events = if failed {
            Vec::new()
        } else {
            vec![Event {
                time_unix_nano: end + 10_000_000,
                name: crate::evalevent::EVENT_NAME.into(),
                attributes: vec![
                    kv_str("gen_ai.evaluation.name", "faithfulness"),
                    kv_f64("gen_ai.evaluation.score.value", faithfulness),
                ],
                ..Default::default()
            }]
        };
        spans.push(Span {
            trace_id: trace,
            span_id: llm,
            parent_span_id: root,
            name: format!("chat {model}"),
            kind: 3,
            start_time_unix_nano: retrieved,
            end_time_unix_nano: end,
            attributes: attrs,
            events,
            status: failed.then(|| Status {
                code: status::StatusCode::Error as i32,
                message: "upstream timeout after 30s".into(),
            }),
            ..Default::default()
        });
    }
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv_str("service.name", "support-bot")],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "evald.demo".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    ..Default::default()
                }),
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// `evald demo`: seed `data_dir` (which must be absent or empty) and say what to do next.
pub async fn demo_command(data_dir: &Path) -> anyhow::Result<Seeded> {
    if data_dir.exists() && std::fs::read_dir(data_dir)?.next().is_some() {
        anyhow::bail!(
            "{} already holds data; the demo writes a fresh store, so pass a new --data-dir",
            data_dir.display()
        );
    }
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let (spans, scores) = build(now_ns)?;
    let sessions = spans
        .iter()
        .filter_map(|s| s.session_id.as_deref())
        .collect::<std::collections::HashSet<_>>()
        .len();
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening a store at {}: {e}", data_dir.display()))?;
    let n = store.ingest_batch(spans, &scores).await?;
    // Into Parquet now, so SQL and every report read it the way they read a real store.
    store.seal_now().await?;
    store.compact_now().await?;
    Ok(Seeded {
        spans: n,
        scores: scores.len(),
        sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_is_recent_priced_scored_and_has_a_coding_agent_session() {
        let now = 1_900_000_000_000_000_000u64;
        let (spans, scores) = build(now).unwrap();
        let newest = spans.iter().map(|s| s.end_unix_nano).max().unwrap();
        assert_eq!(newest, now - 60_000_000_000, "ends a minute before now");
        let agent_end = spans
            .iter()
            .filter(|s| s.name.starts_with("claude_code."))
            .map(|s| s.end_unix_nano)
            .max()
            .unwrap();
        assert_eq!(
            agent_end,
            now - 25 * 60_000_000_000,
            "the session, 25 minutes ago"
        );
        assert!(spans.iter().all(|s| s.start_unix_nano <= s.end_unix_nano));
        assert!(spans.iter().any(|s| s.name == "claude_code.llm_request"));
        let app_llm: Vec<_> = spans
            .iter()
            .filter(|s| {
                s.service_name.as_deref() == Some("support-bot")
                    && s.oi_kind.as_deref() == Some("LLM")
            })
            .collect();
        assert_eq!(app_llm.len(), 3);
        assert!(
            app_llm.iter().filter(|s| s.status_code == 2).count() == 1,
            "one failed call"
        );
        assert!(
            app_llm.iter().any(|s| s.cost_usd.is_some()),
            "priced from the built-in table"
        );
        assert_eq!(
            scores.len(),
            2,
            "a faithfulness score on each successful call"
        );
        let sessions = crate::sessions::rollups(&spans, false);
        assert_eq!(
            sessions.len(),
            3,
            "the Claude Code session and two app sessions"
        );
    }

    #[tokio::test]
    async fn the_demo_seeds_a_fresh_dir_and_refuses_one_with_data() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("demo");
        let seeded = demo_command(&store_dir).await.unwrap();
        assert!(seeded.spans > 10);
        assert_eq!(seeded.sessions, 3);
        let again = demo_command(&store_dir).await.unwrap_err().to_string();
        assert!(again.contains("already holds data"), "{again}");
    }
}
