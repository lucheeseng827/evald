//! evald's normalized span model — one internal representation for both the
//! OpenInference and OTel `gen_ai.*` conventions (see PLAN.md §2.1).
//!
//! This is the *normalized*, ergonomic view produced by [`crate::normalize`], not the
//! wire format and not (yet) the on-disk format. Ids are rendered as lowercase hex
//! strings here for readable logs/JSON; the storage layer (step 4) may keep raw bytes.
//! Every original attribute is preserved losslessly in [`NormalizedSpan::raw_attributes`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Which semantic convention a span appears to have been emitted under. Informational:
/// the normalizer reads *both* conventions' keys regardless, so a `Mixed` or `Unknown`
/// span is still normalized as far as its attributes allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// `openinference.span.kind` / `llm.*` present.
    OpenInference,
    /// `gen_ai.*` present.
    GenAi,
    /// Both conventions present on the same span.
    Mixed,
    /// Neither — a non-LLM span (HTTP client, DB, etc.) or an unrecognized producer.
    Unknown,
}

impl Dialect {
    /// Stable string form (matches the serde `snake_case` rename), used for the
    /// Parquet `dialect` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Dialect::OpenInference => "open_inference",
            Dialect::GenAi => "gen_ai",
            Dialect::Mixed => "mixed",
            Dialect::Unknown => "unknown",
        }
    }

    /// Parse [`Dialect::as_str`]; anything unrecognized maps to [`Dialect::Unknown`].
    pub fn from_str_lenient(s: &str) -> Dialect {
        match s {
            "open_inference" => Dialect::OpenInference,
            "gen_ai" => Dialect::GenAi,
            "mixed" => Dialect::Mixed,
            _ => Dialect::Unknown,
        }
    }
}

/// Token usage, unified across OpenInference (`llm.token_count.*`) and gen_ai
/// (`gen_ai.usage.*`). Every field is mapped from BOTH conventions (PLAN.md §1.4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
}

impl Tokens {
    /// True when no token field is populated (used to omit the block from output).
    pub fn is_empty(&self) -> bool {
        self.prompt.is_none()
            && self.completion.is_none()
            && self.total.is_none()
            && self.cache_read.is_none()
            && self.cache_write.is_none()
            && self.reasoning.is_none()
    }
}

/// A span after normalization: identity + the LLM-semantic fields unified across
/// conventions, plus a lossless copy of every raw attribute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedSpan {
    /// Best guess at the source convention (informational).
    pub dialect: Dialect,
    /// 16-byte trace id, lowercase hex.
    pub trace_id: String,
    /// 8-byte span id, lowercase hex.
    pub span_id: String,
    /// Parent span id (hex), absent for a root span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub name: String,
    /// The OTel `Span.kind` enum (CLIENT/SERVER/INTERNAL/…) as its wire integer — NOT
    /// the LLM span kind, which lives in [`Self::oi_kind`].
    pub otel_kind: i32,
    /// `openinference.span.kind` (LLM | CHAIN | RETRIEVER | EMBEDDING | TOOL | AGENT |
    /// RERANKER | GUARDRAIL | EVALUATOR), read from the attribute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oi_kind: Option<String>,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    /// OTel status code: 0 Unset, 1 Ok, 2 Error.
    pub status_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// Unified model name (gen_ai response/request model, else `llm.model_name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Unified provider (`gen_ai.provider.name`, deprecated `gen_ai.system`, or OI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Tokens::is_empty")]
    pub tokens: Tokens,
    /// USD cost when explicitly reported (`llm.cost.total`). gen_ai carries no cost
    /// attribute; deriving it from a bundled price table is deferred (PLAN.md §2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Captured input (OI `input.value`, else a gen_ai message/prompt string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_value: Option<String>,
    /// Captured output (OI `output.value`, else a gen_ai message/completion string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_version: Option<String>,
    /// Every span attribute, preserved losslessly (key → JSON value). Sorted for
    /// deterministic output.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub raw_attributes: BTreeMap<String, serde_json::Value>,
}

/// Why an LLM-kind span carries no resolvable token usage. Emitted as a read-time
/// diagnostic ([`NormalizedSpan::usage_missing`]) so a silent `0 tokens / $0` is
/// distinguishable from "usage was reported but we could not read it" — the recurring
/// cross-framework pain where token counts live nested inside a payload and never reach
/// the standard `llm.token_count.*` / `gen_ai.usage.*` keys (a recurring cross-framework gap
/// where usage is nested inside a provider payload and never mapped out). Computed from already-stored
/// fields, so it is identical whether a span is served from the hot tier or read back
/// from cold Parquet — no persisted column, no schema/upgrade risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageMissingReason {
    /// No usage found anywhere on the span — neither the standard token keys nor any
    /// nested usage-shaped attribute. (Also the observed shape when a span-filtering
    /// processor dropped the carrier span; that cause is not separately detectable here.)
    NoUsageField,
    /// A usage-shaped attribute IS present (e.g. a nested `usage` / `models_usage` blob
    /// carrying token counts) but no standard `token_count.*` / `gen_ai.usage.*` key was
    /// parseable — usage exists on the span but was never promoted to a readable key.
    UnparsedNesting,
}

impl UsageMissingReason {
    /// Stable string form (matches the serde `kebab-case` rename).
    pub fn as_str(&self) -> &'static str {
        match self {
            UsageMissingReason::NoUsageField => "no-usage-field",
            UsageMissingReason::UnparsedNesting => "unparsed-nesting",
        }
    }
}

impl NormalizedSpan {
    /// Span wall-clock duration in nanoseconds (saturating).
    pub fn duration_ns(&self) -> u64 {
        self.end_unix_nano.saturating_sub(self.start_unix_nano)
    }

    /// Whether this looks like an LLM *generation* span — the only kind for which absent
    /// token usage is worth flagging. Retriever/tool/chain/embedding-less spans that
    /// legitimately have no tokens are excluded, so the diagnostic does not cry wolf.
    pub fn is_llm_span(&self) -> bool {
        // OpenInference marks the kind explicitly.
        if matches!(self.oi_kind.as_deref(), Some(k) if k.eq_ignore_ascii_case("LLM")) {
            return true;
        }
        // A resolved model name is a strong LLM-call signal for gen_ai/OTel spans that
        // don't carry an OpenInference kind.
        if self.model.is_some() {
            return true;
        }
        // gen_ai request/response/usage attributes indicate a model call even when the
        // model name didn't resolve; `gen_ai.tool.*`-only spans deliberately don't match.
        self.raw_attributes.keys().any(|k| {
            k.starts_with("gen_ai.request.")
                || k.starts_with("gen_ai.response.")
                || k.starts_with("gen_ai.usage.")
        })
    }

    /// Classify token-usage presence. `None` = not applicable (non-LLM span) or usage is
    /// present. `Some(reason)` = an LLM span with no resolvable token usage — surface it
    /// instead of silently reporting `0`.
    pub fn usage_missing(&self) -> Option<UsageMissingReason> {
        if !self.is_llm_span() || !self.tokens.is_empty() {
            return None;
        }
        // Usage may have been reported nested (AutoGen `models_usage`, a `usage` blob, a
        // message payload) but not promoted to a standard key. Detect that so the operator
        // sees "unpromoted" rather than "genuinely absent".
        let nested = self.raw_attributes.iter().any(|(k, v)| {
            let kl = k.to_ascii_lowercase();
            (kl.contains("usage") || kl.contains("token")) && json_mentions_tokens(v)
        });
        Some(if nested {
            UsageMissingReason::UnparsedNesting
        } else {
            UsageMissingReason::NoUsageField
        })
    }
}

/// Heuristic: does this JSON value carry token-count-shaped data? Used to tell an
/// "unparsed nested usage" span from a genuinely usage-free one. Scans object keys a few
/// levels deep for `*token*` keys with a numeric-ish value.
fn json_mentions_tokens(v: &serde_json::Value) -> bool {
    fn walk(v: &serde_json::Value, depth: u8) -> bool {
        if depth == 0 {
            return false;
        }
        match v {
            serde_json::Value::Object(map) => map.iter().any(|(k, val)| {
                (k.to_ascii_lowercase().contains("token") && (val.is_number() || val.is_string()))
                    || walk(val, depth - 1)
            }),
            serde_json::Value::Array(arr) => arr.iter().any(|val| walk(val, depth - 1)),
            _ => false,
        }
    }
    walk(v, 4)
}

// --- the universal eval object (PLAN.md §2.2) ------------------------------------

/// What a [`Score`] is attached to. evald is OTel-native, so a span/trace id IS the
/// join key — online, offline, and human scores all target the same id space and share
/// one schema. Serializes flat as `{ "target_type": "span", "target_id": "<hex>" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target_type", content = "target_id", rename_all = "snake_case")]
pub enum ScoreTarget {
    Trace(String),
    Span(String),
    Session(String),
    Run(String),
}

impl ScoreTarget {
    /// Stable index key, e.g. `span:0123…`. Used as the `by_target` redb key.
    pub fn key(&self) -> String {
        match self {
            ScoreTarget::Trace(id) => format!("trace:{id}"),
            ScoreTarget::Span(id) => format!("span:{id}"),
            ScoreTarget::Session(id) => format!("session:{id}"),
            ScoreTarget::Run(id) => format!("run:{id}"),
        }
    }
}

/// The shape of a score's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    #[default]
    Numeric,
    Categorical,
    Boolean,
    Text,
}

/// Who produced a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreSource {
    /// An evaluator (deterministic scorer, LLM-as-judge).
    Eval,
    /// A human annotation.
    Human,
    /// Written directly via the API.
    #[default]
    Api,
}

/// Sufficient statistics for an evaluator's aggregate over one eval run — the per-run
/// summary `eval compare` needs to test whether a run-to-run change in the mean is
/// statistically significant (Welch's t) rather than sampling noise. Carried only on the
/// run-targeted aggregate Scores (`{run_id}:agg:{name}`); `None` on every per-item / human /
/// API score, and `None` on aggregates written before this field existed (serde default →
/// fully backward-compatible with already-persisted runs, which then compare on the mean
/// alone with no significance verdict).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AggStats {
    /// Number of items the evaluator scored (the sample size). `mean`/`variance` are over
    /// these scored items only (skipped items are excluded).
    pub n: u64,
    /// How many of the scored items passed — the pass count behind the pass-rate.
    pub pass_count: u64,
    /// Sample mean of the per-item values (equals the aggregate Score's `num_value`;
    /// duplicated here so [`AggStats`] is self-contained for the test).
    pub mean: f64,
    /// Unbiased (ddof = 1) sample variance of the per-item values; `0.0` when `n < 2`.
    pub variance: f64,
}

/// A score/annotation attached to a target. One schema for eval results, human
/// annotations, and API-supplied scores; `config_id` (a future [`ScoreConfig`], a Beta
/// item) is carried forward for schema enforcement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub id: String,
    #[serde(flatten)]
    pub target: ScoreTarget,
    pub name: String,
    /// Numeric value (numeric/boolean scores).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_value: Option<f64>,
    /// String value (categorical label / free text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub str_value: Option<String>,
    pub data_type: DataType,
    pub source: ScoreSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_id: Option<String>,
    /// Aggregate sufficient statistics — present only on eval-run aggregate Scores; enables
    /// the significance test in `eval compare`. Optional + defaulted, so the on-disk Score
    /// format stays backward/forward-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agg_stats: Option<AggStats>,
    pub ts_unix_nano: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal LLM-kind span with no tokens, for the usage-diagnostic tests.
    fn llm_span() -> NormalizedSpan {
        NormalizedSpan {
            dialect: Dialect::OpenInference,
            trace_id: "aa".repeat(16),
            span_id: "bb".repeat(8),
            parent_span_id: None,
            name: "llm.call".into(),
            otel_kind: 3,
            oi_kind: Some("LLM".into()),
            start_unix_nano: 0,
            end_unix_nano: 10,
            status_code: 0,
            status_message: None,
            model: Some("gpt-4o".into()),
            provider: Some("openai".into()),
            tokens: Tokens::default(),
            cost_usd: None,
            input_value: None,
            output_value: None,
            session_id: None,
            user_id: None,
            service_name: None,
            scope_name: None,
            scope_version: None,
            raw_attributes: BTreeMap::new(),
        }
    }

    #[test]
    fn usage_missing_flags_llm_span_without_tokens() {
        let s = llm_span();
        assert!(s.is_llm_span());
        assert_eq!(s.usage_missing(), Some(UsageMissingReason::NoUsageField));
    }

    #[test]
    fn usage_present_is_not_flagged() {
        let mut s = llm_span();
        s.tokens.prompt = Some(10);
        s.tokens.completion = Some(5);
        assert_eq!(s.usage_missing(), None);
    }

    #[test]
    fn non_llm_span_is_never_flagged() {
        let mut s = llm_span();
        s.oi_kind = Some("RETRIEVER".into());
        s.model = None; // a retriever has no model / gen_ai.* usage keys
        assert!(!s.is_llm_span());
        assert_eq!(s.usage_missing(), None);
    }

    #[test]
    fn nested_usage_blob_is_unparsed_nesting_not_absent() {
        // AutoGen-style: token counts live nested under `models_usage`, never promoted
        // to llm.token_count.* — usage exists but is unreadable.
        let mut s = llm_span();
        s.raw_attributes.insert(
            "models_usage".into(),
            serde_json::json!({ "prompt_tokens": 1200, "completion_tokens": 300 }),
        );
        assert_eq!(s.usage_missing(), Some(UsageMissingReason::UnparsedNesting));
    }

    #[test]
    fn genai_request_attr_makes_it_an_llm_span_even_without_model() {
        let mut s = llm_span();
        s.oi_kind = None;
        s.model = None;
        s.raw_attributes
            .insert("gen_ai.request.model".into(), serde_json::json!("gpt-4o"));
        assert!(s.is_llm_span());
        assert_eq!(s.usage_missing(), Some(UsageMissingReason::NoUsageField));
    }

    #[test]
    fn usage_missing_reason_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&UsageMissingReason::UnparsedNesting).unwrap(),
            "\"unparsed-nesting\""
        );
        assert_eq!(UsageMissingReason::NoUsageField.as_str(), "no-usage-field");
    }

    #[test]
    fn score_without_agg_stats_field_deserializes() {
        // A record written by a binary that predates `agg_stats` — the key is simply absent.
        // It must load (agg_stats -> None) so old `--data-dir`s keep working.
        let legacy = r#"{"id":"x","target_type":"run","target_id":"r1","name":"exact_match",
            "num_value":0.9,"data_type":"numeric","source":"eval","ts_unix_nano":1}"#;
        let s: Score = serde_json::from_str(legacy).expect("legacy Score must deserialize");
        assert_eq!(s.agg_stats, None);
        assert_eq!(s.num_value, Some(0.9));
        // Forward-compat: a None agg_stats serializes WITHOUT the key (skip_serializing_if), so
        // a new binary's output stays readable by an old one.
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("agg_stats"),
            "key must be omitted when None: {json}"
        );
    }

    #[test]
    fn score_with_agg_stats_round_trips_exactly() {
        let s = Score {
            id: "x".into(),
            target: ScoreTarget::Run("r1".into()),
            name: "exact_match".into(),
            num_value: Some(0.8),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Eval,
            comment: None,
            config_id: None,
            agg_stats: Some(AggStats {
                n: 10,
                pass_count: 8,
                mean: 0.8,
                variance: 0.16,
            }),
            ts_unix_nano: 5,
        };
        let back: Score = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }
}
