//! `evald replay`: a dataset's prompts sent to another endpoint, answered the way the migration
//! gate needs them: one call per item, at temperature 0 with a fixed seed and no response format,
//! through the egress door. The answers become the outputs of an ordinary eval run, the
//! candidate run, scored by the config's own evaluators, so `eval compare --non-inferior` pairs it
//! with the baseline item by item.
//!
//! Each call is an `evald.replay` span (a model call like any other: cost, tokens, latency), and
//! what the replay was and what it took goes in the candidate run's manifest as `replay`.

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::endpoint::{Endpoint, StructuredOutput};
use crate::eval::DatasetItem;
use crate::judge::{JudgeBackend, JudgeCall, Provenance};
use crate::model::NormalizedSpan;

/// The `service.name` of the spans a replay writes for its calls.
pub const REPLAY_SERVICE: &str = "evald.replay";

/// One item's call: what was sent, what came back, and what it took.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayCall {
    /// The item's key ([`DatasetItem::key`]), which the candidate run keeps.
    pub key: String,
    pub prompt: String,
    pub text: String,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    pub usage: Option<(u64, u64)>,
    /// The model the server says answered.
    pub model: Option<String>,
    pub attempts: u32,
}

/// What a replay was and what it took, as the candidate run's manifest records it (`replay`):
/// the endpoint and the weights behind it, and the calls' tokens, cost and latency, which
/// `eval compare` sets beside the baseline's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplaySummary {
    pub endpoint: String,
    pub base_url: String,
    #[serde(default)]
    pub jurisdiction: Option<String>,
    /// The model name the calls asked for.
    pub model: String,
    /// The models the server says answered.
    #[serde(default)]
    pub answered_as: Vec<String>,
    /// Which weights those were, where the server says.
    #[serde(default)]
    pub provenance: Option<Provenance>,
    pub calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The calls' cost at the price table's rates (`--price-table` prices a model the table
    /// does not, such as a self-hosted one at its GPU rate); `None` when no call was priced.
    pub cost_usd: Option<f64>,
    /// Calls the price table had no rate for.
    pub unpriced_calls: usize,
    pub latency_ms_p50: f64,
    pub latency_ms_p95: f64,
}

impl ReplaySummary {
    /// Price the replay's tokens at `table`'s base rates for the model that answered (else the
    /// one asked for), when the table prices it: `eval compare --price-table` sets both sides at
    /// the rates you give it, whatever the table was when each replay ran.
    pub fn reprice(&mut self, table: &crate::price::PriceTable) {
        let model = self.answered_as.first().unwrap_or(&self.model);
        if let Some((_, price)) = table.lookup(model) {
            let (input, output) = price.base_rates();
            self.cost_usd =
                Some(self.input_tokens as f64 * input + self.output_tokens as f64 * output);
            self.unpriced_calls = 0;
        }
    }
}

/// Refuse a dataset that cannot be replayed, before any call: every item needs a key (so the
/// candidate pairs with the baseline) and an input (the prompt to send).
pub fn check_items(items: &[DatasetItem]) -> anyhow::Result<()> {
    anyhow::ensure!(!items.is_empty(), "the dataset has no items to replay");
    let mut keys = std::collections::BTreeSet::new();
    for (i, item) in items.iter().enumerate() {
        let key = item.key().with_context(|| {
            format!(
                "item {} has no id, span_id or trace_id: a replayed item must pair with the \
                 baseline's, so give dataset items an `id`",
                i + 1
            )
        })?;
        anyhow::ensure!(
            item.input.as_deref().is_some_and(|s| !s.is_empty()),
            "item {key:?} has no input to replay"
        );
        anyhow::ensure!(
            keys.insert(key),
            "item key {key:?} repeats: a replayed item must pair with exactly one baseline item"
        );
    }
    Ok(())
}

/// Send each item's `input` to `model` on `ep` as the user message, up to the endpoint's
/// `max_in_flight` at once, and keep dataset order. Stops at the first call that fails after
/// the endpoint's retries.
pub async fn replay_items<B: JudgeBackend>(
    backend: &B,
    ep: &Endpoint,
    model: &str,
    items: &[DatasetItem],
) -> anyhow::Result<Vec<ReplayCall>> {
    use futures::{StreamExt, TryStreamExt};
    check_items(items)?;
    // The deterministic parameters of a judge call, without asking for a judge's JSON.
    let ep = Endpoint {
        structured_output: StructuredOutput::None,
        ..ep.clone()
    };
    let ep = &ep;
    futures::stream::iter(items.iter().map(|item| async move {
        let key = item.key().expect("checked").to_string();
        let prompt = item.input.clone().expect("checked");
        let start = crate::judge::unix_nanos_now();
        let reply = backend
            .complete(
                ep,
                JudgeCall {
                    model,
                    system: "",
                    user: &prompt,
                },
            )
            .await
            .with_context(|| format!("replaying item {key:?}"))?;
        Ok::<_, anyhow::Error>(ReplayCall {
            key,
            prompt,
            text: reply.text,
            start_unix_nano: start,
            end_unix_nano: crate::judge::unix_nanos_now().max(start),
            usage: reply.usage,
            model: reply.model,
            attempts: reply.attempts,
        })
    }))
    .buffered(ep.max_in_flight.max(1))
    .try_collect()
    .await
}

/// The trace of a replay run, and the span of one item's call in it (hex).
pub fn span_ids(run_id: &str, key: &str) -> (String, String) {
    use crate::logs::derive_id;
    (
        hex::encode(derive_id(16, &["evald.replay.run", run_id])),
        hex::encode(derive_id(8, &["evald.replay.call", run_id, key])),
    )
}

/// Each call as an ordinary GenAI span (`service.name = evald.replay`, one trace per run), with
/// the prompt and the answer, normalised (and priced) the way an app's spans are.
pub fn spans(
    run_id: &str,
    ep: &Endpoint,
    model: &str,
    calls: &[ReplayCall],
) -> Vec<NormalizedSpan> {
    use crate::logs::{derive_id, kv_int, kv_str};
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{span::SpanKind, ResourceSpans, ScopeSpans, Span};

    if calls.is_empty() {
        return Vec::new();
    }
    let host = ep
        .base_url
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    let trace_id = derive_id(16, &["evald.replay.run", run_id]);
    let spans = calls
        .iter()
        .map(|c| {
            let mut attributes = vec![
                kv_str("gen_ai.operation.name", "chat"),
                kv_str("gen_ai.provider.name", &ep.name),
                kv_str("gen_ai.request.model", model),
                kv_str("server.address", &host),
                kv_str("evald.run_id", run_id),
                kv_str("evald.replay.endpoint", &ep.name),
                kv_str("evald.replay.item", &c.key),
                kv_int("evald.replay.attempts", i64::from(c.attempts)),
                kv_str("input.value", &c.prompt),
                kv_str("output.value", &c.text),
            ];
            if let Some(m) = &c.model {
                attributes.push(kv_str("gen_ai.response.model", m));
            }
            if let Some((input, output)) = c.usage {
                attributes.push(kv_int("gen_ai.usage.input_tokens", input as i64));
                attributes.push(kv_int("gen_ai.usage.output_tokens", output as i64));
            }
            Span {
                trace_id: trace_id.clone(),
                span_id: derive_id(8, &["evald.replay.call", run_id, &c.key]),
                name: format!("chat {model}"),
                kind: SpanKind::Client as i32,
                start_time_unix_nano: c.start_unix_nano,
                end_time_unix_nano: c.end_unix_nano,
                attributes,
                ..Default::default()
            }
        })
        .collect();
    let req = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv_str("service.name", REPLAY_SERVICE)],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: REPLAY_SERVICE.into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    ..Default::default()
                }),
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    crate::normalize::normalize_request(&req)
}

/// The nearest-rank `q` quantile of `sorted` (ascending, non-empty).
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// The run's [`ReplaySummary`], from its normalised spans (which carry the price table's cost).
pub fn summary(
    ep: &Endpoint,
    model: &str,
    provenance: Option<Provenance>,
    spans: &[NormalizedSpan],
) -> ReplaySummary {
    let mut latencies: Vec<f64> = spans
        .iter()
        .map(|s| s.end_unix_nano.saturating_sub(s.start_unix_nano) as f64 / 1e6)
        .collect();
    latencies.sort_by(f64::total_cmp);
    let (p50, p95) = if latencies.is_empty() {
        (0.0, 0.0)
    } else {
        (quantile(&latencies, 0.5), quantile(&latencies, 0.95))
    };
    let priced: Vec<f64> = spans.iter().filter_map(|s| s.cost_usd).collect();
    ReplaySummary {
        endpoint: ep.name.clone(),
        base_url: ep.base_url.clone(),
        jurisdiction: ep.jurisdiction.clone(),
        model: model.to_string(),
        answered_as: spans
            .iter()
            .filter_map(|s| s.model.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
        provenance,
        calls: spans.len(),
        input_tokens: spans.iter().filter_map(|s| s.tokens.prompt).sum(),
        output_tokens: spans.iter().filter_map(|s| s.tokens.completion).sum(),
        cost_usd: (!priced.is_empty()).then(|| priced.iter().sum()),
        unpriced_calls: spans.len() - priced.len(),
        latency_ms_p50: p50,
        latency_ms_p95: p95,
    }
}

/// The candidate run's items: each item as it was, answered by the replay. It keeps the item's
/// key as its `id`, so it pairs with the baseline, and points at its own replay span, so its
/// scores land on the call that produced it and not on the baseline's span.
pub fn candidates(items: &[DatasetItem], calls: &[ReplayCall], run_id: &str) -> Vec<DatasetItem> {
    items
        .iter()
        .zip(calls)
        .map(|(item, call)| {
            let (trace_id, span_id) = span_ids(run_id, &call.key);
            DatasetItem {
                id: Some(call.key.clone()),
                output: call.text.clone(),
                span_id: Some(span_id),
                trace_id: Some(trace_id),
                ..item.clone()
            }
        })
        .collect()
}

/// Print what the replay took, after the run's report.
pub fn print_summary(s: &ReplaySummary) {
    let weights = s
        .provenance
        .as_ref()
        .map(Provenance::describe)
        .filter(|d| !d.is_empty())
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    println!(
        "\nreplayed on {} ({}), model {}{weights}",
        s.endpoint, s.base_url, s.model
    );
    let cost = match s.cost_usd {
        Some(c) if s.unpriced_calls == 0 => format!("${c:.4}"),
        Some(c) => format!("${c:.4} + {} unpriced call(s)", s.unpriced_calls),
        None => "unpriced (give its rate with --price-table)".to_string(),
    };
    println!(
        "  {} call(s), {} input + {} output tokens, {cost}, latency p50 {:.0} ms, p95 {:.0} ms",
        s.calls, s.input_tokens, s.output_tokens, s.latency_ms_p50, s.latency_ms_p95
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::Api;
    use crate::judge::MockBackend;

    fn ep() -> Endpoint {
        Endpoint {
            name: "gpu-box".into(),
            api: Api::OpenaiChat,
            base_url: "http://gpu:8000".into(),
            api_key_env: None,
            ca_file: None,
            client_cert: None,
            client_key: None,
            timeout: std::time::Duration::from_secs(5),
            max_in_flight: 2,
            headers: Default::default(),
            structured_output: StructuredOutput::Auto,
            retries: 0,
            jurisdiction: Some("MY".into()),
        }
    }

    fn item(id: Option<&str>, input: Option<&str>) -> DatasetItem {
        DatasetItem {
            id: id.map(String::from),
            input: input.map(String::from),
            output: "the baseline's answer".into(),
            expected_output: Some("4".into()),
            span_id: Some("baselinespan0001".into()),
            trace_id: None,
            context: None,
            metadata: Some(serde_json::json!({"language": "ms"})),
        }
    }

    #[tokio::test]
    async fn a_replay_answers_every_item_and_the_candidate_pairs_with_the_baseline() {
        let backend = MockBackend::new("4").with_identity("qwen2.5:7b");
        let items = vec![
            item(Some("q1"), Some("2+2?")),
            item(Some("q2"), Some("3+1?")),
        ];
        let calls = replay_items(&backend, &ep(), "qwen2.5:7b", &items)
            .await
            .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (
                calls[0].key.as_str(),
                calls[0].prompt.as_str(),
                calls[0].text.as_str()
            ),
            ("q1", "2+2?", "4")
        );
        assert_eq!(backend.call_count(), 2);

        let cand = candidates(&items, &calls, "run-b");
        assert_eq!(cand[1].id.as_deref(), Some("q2"));
        assert_eq!(cand[1].output, "4");
        assert_eq!(cand[1].expected_output.as_deref(), Some("4"));
        assert_eq!(
            cand[1].metadata, items[1].metadata,
            "slices travel with the item"
        );
        // Its scores go on the replay's span, not the baseline's.
        let spans = spans("run-b", &ep(), "qwen2.5:7b", &calls);
        assert_eq!(cand[1].span_id.as_deref(), Some(spans[1].span_id.as_str()));
        assert_eq!(
            cand[1].trace_id.as_deref(),
            Some(spans[1].trace_id.as_str())
        );
        assert_ne!(cand[1].span_id.as_deref(), Some("baselinespan0001"));

        let s = &spans[0];
        assert_eq!(s.service_name.as_deref(), Some(REPLAY_SERVICE));
        assert!(s.is_llm_span(), "a replay call is a model call");
        assert_eq!(s.tokens.prompt, Some(10));
        assert_eq!(s.input_value.as_deref(), Some("2+2?"));
        assert_eq!(s.output_value.as_deref(), Some("4"));
        assert_eq!(s.raw_attributes["evald.replay.item"], "q1");
        assert_eq!(s.raw_attributes["evald.run_id"], "run-b");

        let sum = summary(&ep(), "qwen2.5:7b", None, &spans);
        assert_eq!(sum.calls, 2);
        assert_eq!((sum.input_tokens, sum.output_tokens), (20, 10));
        assert_eq!(sum.answered_as, ["qwen2.5:7b"]);
        assert_eq!(sum.jurisdiction.as_deref(), Some("MY"));
        assert_eq!(
            (sum.cost_usd, sum.unpriced_calls),
            (None, 2),
            "a self-hosted model the table does not price"
        );
    }

    #[tokio::test]
    async fn a_dataset_that_cannot_pair_is_refused_before_any_call() {
        let backend = MockBackend::new("4");
        for (items, want) in [
            (vec![], "no items"),
            (
                vec![DatasetItem {
                    span_id: None,
                    ..item(None, Some("x"))
                }],
                "give dataset items an `id`",
            ),
            (vec![item(Some("q1"), None)], "no input"),
            (vec![item(Some("q1"), Some(""))], "no input"),
            (
                vec![item(Some("q1"), Some("a")), item(Some("q1"), Some("b"))],
                "repeats",
            ),
        ] {
            let e = replay_items(&backend, &ep(), "m", &items)
                .await
                .unwrap_err();
            assert!(format!("{e:#}").contains(want), "{e:#}");
        }
        assert_eq!(backend.call_count(), 0);
    }

    #[test]
    fn a_replay_is_repriced_at_the_rates_given() {
        let mut s = ReplaySummary {
            endpoint: "gpu-box".into(),
            base_url: "http://gpu:8000".into(),
            jurisdiction: None,
            model: "qwen2.5:7b".into(),
            answered_as: vec![],
            provenance: None,
            calls: 10,
            input_tokens: 1_000,
            output_tokens: 100,
            cost_usd: None,
            unpriced_calls: 10,
            latency_ms_p50: 1.0,
            latency_ms_p95: 2.0,
        };
        let table = crate::price::PriceTable::from_json(
            r#"{"qwen2.5:7b": {"input_cost_per_token": 1e-7, "output_cost_per_token": 3e-7}}"#,
        )
        .unwrap();
        s.reprice(&table);
        assert!((s.cost_usd.unwrap() - (1e-4 + 3e-5)).abs() < 1e-12);
        assert_eq!(s.unpriced_calls, 0);
        // A model the table does not price keeps what it had.
        let mut other = ReplaySummary {
            model: "unknown-model".into(),
            cost_usd: Some(0.5),
            ..s.clone()
        };
        other.reprice(&crate::price::PriceTable::from_json("{}").unwrap());
        assert_eq!(other.cost_usd, Some(0.5));
    }

    #[test]
    fn latency_quantiles_are_nearest_rank() {
        let v: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(quantile(&v, 0.5), 10.0);
        assert_eq!(quantile(&v, 0.95), 19.0);
        assert_eq!(quantile(&[7.0], 0.95), 7.0);
    }
}
